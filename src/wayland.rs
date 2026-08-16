use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::ffi::CString;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::future::join_all;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wayrs_client::global::{BindError, GlobalExt};
use wayrs_client::object::{ObjectId, Proxy};
use wayrs_client::protocol::{WlSeat, wl_registry};
use wayrs_client::{Connection, EventCtx};
use wayrs_protocols::wlr_data_control_unstable_v1::{
    ZwlrDataControlDeviceV1, ZwlrDataControlManagerV1, ZwlrDataControlOfferV1,
    ZwlrDataControlSourceV1, zwlr_data_control_device_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use crate::api::{Request, Response};
use crate::config::{
    API_TIMEOUT, MAX_ITEM_BYTES, MIME_INACTIVITY_TIMEOUT, SENSITIVE_ACTIVATION_TIMEOUT,
};
use crate::storage::{IncomingFormat, IncomingItem, Store};

#[derive(Debug)]
struct Offer {
    proxy: ZwlrDataControlOfferV1,
    mime_types: Vec<String>,
}

#[derive(Debug)]
struct Seat {
    seat: WlSeat,
    device: ZwlrDataControlDeviceV1,
    offers: HashMap<ObjectId, Offer>,
    active_source: Option<ActiveSource>,
    ignore_next_selection: bool,
}

#[derive(Debug)]
struct ActiveSource {
    proxy: ZwlrDataControlSourceV1,
    item_id: String,
    payloads: HashMap<String, std::fs::File>,
}

#[derive(Debug)]
struct State {
    manager: ZwlrDataControlManagerV1,
    seats: HashMap<u32, Seat>,
    selections: VecDeque<Selection>,
    sensitive_expiration: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
enum Selection {
    Offer { seat_name: u32, offer: Offer },
    Clear { seat_name: u32 },
}

pub async fn observe(store: &Store, assume_ownership: bool) -> Result<(), WaylandError> {
    let mut connection = Connection::<Infallible>::connect()?;
    connection.async_roundtrip().await?;
    let manager = connection.bind_singleton::<ZwlrDataControlManagerV1>(1..=2)?;
    #[expect(deprecated)]
    let mut connection = connection.clear_callbacks::<State>();
    let mut state = State {
        manager,
        seats: HashMap::new(),
        selections: VecDeque::new(),
        sensitive_expiration: None,
    };

    connection.add_registry_cb(registry_event);
    connection.dispatch_events(&mut state);
    connection.async_flush().await?;
    let (listener, _socket_guard) = bind_api_socket()?;
    let (expiration_tx, mut expiration_rx) = mpsc::unbounded_channel();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    info!("watching the regular clipboard through wlr-data-control");

    loop {
        tokio::select! {
            result = connection.async_recv_events() => {
                result?;
                connection.dispatch_events(&mut state);
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                handle_api(
                    stream,
                    &mut connection,
                    &mut state,
                    store,
                    assume_ownership,
                    expiration_tx.clone(),
                ).await;
                connection.async_flush().await?;
                continue;
            }
            Some(expired_id) = expiration_rx.recv() => {
                clear_active_item(&mut connection, &mut state, &expired_id);
                connection.async_flush().await?;
                continue;
            }
            result = &mut shutdown => {
                result?;
                info!("clipboard observer stopped");
                return Ok(());
            }
        }

        while let Some(selection) = state.selections.pop_front() {
            match selection {
                Selection::Offer { seat_name, offer } => {
                    let format_count = offer.mime_types.len();
                    let captured = capture_offer(&mut connection, offer).await;
                    match captured {
                        Ok((temporary, incoming)) if !incoming.formats.is_empty() => {
                            let complete = incoming.complete;
                            let stored_formats = incoming.formats.len();
                            let entry = store.store(incoming)?;
                            info!(
                                seat = seat_name,
                                id = &entry.id[..12],
                                formats = stored_formats,
                                advertised_formats = format_count,
                                complete,
                                "stored clipboard item"
                            );
                            drop(temporary);
                            if assume_ownership && entry.complete && !entry.sensitive {
                                cancel_sensitive_expiration(&mut state);
                                own_selection(
                                    &mut connection,
                                    &mut state,
                                    store,
                                    seat_name,
                                    &entry.id,
                                )?;
                                connection.async_flush().await?;
                            }
                        }
                        Ok((_temporary, _incoming)) => {
                            warn!(seat = seat_name, "clipboard offer had no readable formats");
                        }
                        Err(error) => {
                            warn!(seat = seat_name, %error, "failed to capture clipboard offer");
                        }
                    }
                }
                Selection::Clear { seat_name } => {
                    info!(seat = seat_name, "clipboard cleared");
                }
            }
        }

        connection.async_flush().await?;
    }
}

async fn shutdown_signal() -> Result<(), std::io::Error> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

async fn handle_api(
    stream: UnixStream,
    connection: &mut Connection<State>,
    state: &mut State,
    store: &Store,
    assume_ownership: bool,
    expiration_tx: mpsc::UnboundedSender<String>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let response = match tokio::time::timeout(
        API_TIMEOUT,
        tokio::io::BufReader::new(reader).read_line(&mut line),
    )
    .await
    {
        Err(_) => Response::failure("API request timed out"),
        Ok(Err(error)) => Response::failure(error),
        Ok(Ok(0)) => Response::failure("empty API request"),
        Ok(Ok(_)) => match serde_json::from_str::<Request>(&line) {
            Ok(request) => execute_api(
                request,
                connection,
                state,
                store,
                assume_ownership,
                expiration_tx,
            ),
            Err(error) => Response::failure(error),
        },
    };

    let write_result = async {
        let bytes = serde_json::to_vec(&response)?;
        writer.write_all(&bytes).await?;
        writer.write_all(b"\n").await?;
        writer.shutdown().await
    }
    .await;
    if let Err(error) = write_result {
        warn!(%error, "failed to write API response");
    }
}

fn execute_api(
    request: Request,
    connection: &mut Connection<State>,
    state: &mut State,
    store: &Store,
    assume_ownership: bool,
    expiration_tx: mpsc::UnboundedSender<String>,
) -> Response {
    let result = (|| -> Result<Response, Box<dyn std::error::Error>> {
        match request {
            Request::List => Ok(Response::success(store.list_items()?)),
            Request::Show { id } => {
                let id = store.resolve_id(&id)?;
                Ok(Response::success(store.manifest(&id)?))
            }
            Request::Use { id } => {
                let id = store.resolve_id(&id)?;
                let entry = store.activate(&id)?;
                cancel_sensitive_expiration(state);
                let seats = state.seats.keys().copied().collect::<Vec<_>>();
                if seats.is_empty() {
                    return Err("no Wayland seats are available".into());
                }
                for seat_name in seats {
                    own_selection(connection, state, store, seat_name, &id)?;
                }
                if entry.sensitive {
                    state.sensitive_expiration = Some(tokio::spawn(async move {
                        tokio::time::sleep(SENSITIVE_ACTIVATION_TIMEOUT).await;
                        let _ = expiration_tx.send(id);
                    }));
                }
                Ok(Response::success(entry))
            }
            Request::Delete { id } => {
                let id = store.resolve_id(&id)?;
                store.delete(&id)?;
                Ok(Response::empty())
            }
            Request::Clear => {
                store.clear()?;
                Ok(Response::empty())
            }
            Request::Status => {
                let index = store.load_index()?;
                Ok(Response::success(serde_json::json!({
                    "stored_items": index.items.len(),
                    "stored_bytes": index.items.iter().map(|item| item.stored_bytes).sum::<u64>(),
                    "assume_ownership": assume_ownership,
                    "seats": state.seats.len(),
                })))
            }
        }
    })();

    result.unwrap_or_else(Response::failure)
}

fn cancel_sensitive_expiration(state: &mut State) {
    if let Some(expiration) = state.sensitive_expiration.take() {
        expiration.abort();
    }
}

fn clear_active_item(connection: &mut Connection<State>, state: &mut State, item_id: &str) {
    for seat in state.seats.values_mut() {
        if seat
            .active_source
            .as_ref()
            .is_some_and(|source| source.item_id == item_id)
        {
            seat.device.set_selection(connection, None);
            info!(id = &item_id[..12], "expired sensitive clipboard item");
        }
    }
}

fn bind_api_socket() -> Result<(UnixListener, SocketGuard), WaylandError> {
    let path = crate::config::socket_path()?;
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            return Err(WaylandError::AlreadyRunning);
        }
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((listener, SocketGuard(path)))
}

struct SocketGuard(std::path::PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn capture_offer(
    connection: &mut Connection<State>,
    offer: Offer,
) -> Result<(tempfile::TempDir, IncomingItem), CaptureError> {
    let result = capture_offer_data(connection, &offer).await;
    offer.proxy.destroy(connection);
    result
}

async fn capture_offer_data(
    connection: &mut Connection<State>,
    offer: &Offer,
) -> Result<(tempfile::TempDir, IncomingItem), CaptureError> {
    let sensitive = offer
        .mime_types
        .iter()
        .any(|mime| mime.ends_with("x-kde-passwordManagerHint"));
    let temporary = tempfile::tempdir()?;
    let total_bytes = Arc::new(AtomicU64::new(0));
    let requests = offer
        .mime_types
        .iter()
        .cloned()
        .enumerate()
        .collect::<Vec<_>>();
    let first_results = read_round(
        connection,
        offer.proxy,
        temporary.path(),
        &requests,
        Arc::clone(&total_bytes),
    )
    .await?;

    let mut formats = Vec::with_capacity(first_results.len());
    let mut retry = Vec::new();
    for (position, mime, result) in first_results {
        match result {
            Ok(format) => formats.push(format),
            Err(CaptureError::TooLarge(limit)) => {
                return Err(CaptureError::TooLarge(limit));
            }
            Err(error) => {
                debug!(%error, mime, "retrying unreadable clipboard format");
                retry.push((position, mime));
            }
        }
    }

    let successful_bytes = formats.iter().try_fold(0_u64, |total, format| {
        Ok::<_, std::io::Error>(total + std::fs::metadata(&format.path)?.len())
    })?;
    total_bytes.store(successful_bytes, Ordering::Relaxed);

    let retry_results = read_round(
        connection,
        offer.proxy,
        temporary.path(),
        &retry,
        Arc::clone(&total_bytes),
    )
    .await?;

    let mut complete = true;
    for (_position, _mime, result) in retry_results {
        match result {
            Ok(format) => formats.push(format),
            Err(CaptureError::TooLarge(limit)) => return Err(CaptureError::TooLarge(limit)),
            Err(error) => {
                complete = false;
                warn!(%error, "could not read an advertised clipboard format after retry");
            }
        }
    }
    formats.sort_by_key(|format| {
        offer
            .mime_types
            .iter()
            .position(|mime| mime == &format.mime)
            .unwrap_or(usize::MAX)
    });

    Ok((
        temporary,
        IncomingItem {
            formats,
            sensitive,
            complete,
        },
    ))
}

async fn read_round(
    connection: &mut Connection<State>,
    offer: ZwlrDataControlOfferV1,
    directory: &std::path::Path,
    requests: &[(usize, String)],
    total_bytes: Arc<AtomicU64>,
) -> Result<Vec<(usize, String, Result<IncomingFormat, CaptureError>)>, CaptureError> {
    let mut readers = Vec::with_capacity(requests.len());
    for (position, mime) in requests {
        let (reader, writer) = tokio_pipe::pipe()?;
        let fd = unsafe { OwnedFd::from_raw_fd(writer.into_raw_fd()) };
        offer.receive(connection, CString::new(mime.as_str())?, fd);
        let mime = mime.clone();
        let path = directory.join(format!("payload-{position}"));
        let total_bytes = Arc::clone(&total_bytes);
        let position = *position;
        readers.push(async move {
            let result = read_payload(mime.clone(), path, reader, total_bytes).await;
            (position, mime, result)
        });
    }
    connection.async_flush().await?;
    Ok(join_all(readers).await)
}

async fn read_payload(
    mime: String,
    path: std::path::PathBuf,
    mut reader: tokio_pipe::PipeRead,
    total_bytes: Arc<AtomicU64>,
) -> Result<IncomingFormat, CaptureError> {
    let mut file = tokio::fs::File::create(&path).await?;
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let count = tokio::time::timeout(MIME_INACTIVITY_TIMEOUT, reader.read(&mut buffer))
            .await
            .map_err(|_| CaptureError::Timeout(mime.clone()))??;
        if count == 0 {
            break;
        }

        let count_u64 = count as u64;
        total_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current
                    .checked_add(count_u64)
                    .filter(|total| *total <= MAX_ITEM_BYTES)
            })
            .map_err(|_| CaptureError::TooLarge(MAX_ITEM_BYTES))?;
        file.write_all(&buffer[..count]).await?;
    }
    file.flush().await?;

    Ok(IncomingFormat { mime, path })
}

fn own_selection(
    connection: &mut Connection<State>,
    state: &mut State,
    store: &Store,
    seat_name: u32,
    item_id: &str,
) -> Result<(), WaylandError> {
    let manifest = store.manifest(item_id)?;
    let item_directory = store.root().join("items").join(item_id);
    let payloads = manifest
        .formats
        .iter()
        .map(|format| {
            Ok((
                format.mime.clone(),
                std::fs::File::open(item_directory.join(&format.payload))?,
            ))
        })
        .collect::<Result<HashMap<_, _>, std::io::Error>>()?;
    let source = state
        .manager
        .create_data_source_with_cb(connection, move |context| {
            source_event(seat_name, context);
        });
    for format in &manifest.formats {
        source.offer(connection, CString::new(format.mime.as_str())?);
    }

    let Some(seat) = state.seats.get_mut(&seat_name) else {
        source.destroy(connection);
        return Ok(());
    };
    let previous = seat.active_source.replace(ActiveSource {
        proxy: source,
        item_id: item_id.to_owned(),
        payloads,
    });
    seat.ignore_next_selection = true;
    seat.device.set_selection(connection, Some(source));
    if let Some(previous) = previous {
        previous.proxy.destroy(connection);
    }
    info!(
        seat = seat_name,
        id = &item_id[..12],
        "assumed clipboard ownership"
    );
    Ok(())
}

fn source_event(seat_name: u32, context: EventCtx<State, ZwlrDataControlSourceV1>) {
    let source_id = context.proxy.id();
    match context.event {
        zwlr_data_control_source_v1::Event::Send(args) => {
            let Some(source) = context
                .state
                .seats
                .get(&seat_name)
                .and_then(|seat| seat.active_source.as_ref())
                .filter(|source| source.proxy.id() == source_id)
            else {
                return;
            };
            let mime = args.mime_type.to_string_lossy();
            let Some(file) = source.payloads.get(mime.as_ref()) else {
                warn!(seat = seat_name, mime = %mime, "requested unavailable clipboard format");
                return;
            };
            let file = match file.try_clone() {
                Ok(file) => file,
                Err(error) => {
                    warn!(seat = seat_name, mime = %mime, %error, "failed to clone clipboard payload");
                    return;
                }
            };
            let item_id = source.item_id.clone();
            tokio::spawn(async move {
                if let Err(error) = send_payload(file, args.fd).await {
                    warn!(id = &item_id[..12], %error, "failed to serve clipboard payload");
                }
            });
        }
        zwlr_data_control_source_v1::Event::Cancelled => {
            if let Some(seat) = context.state.seats.get_mut(&seat_name)
                && seat
                    .active_source
                    .as_ref()
                    .is_some_and(|source| source.proxy.id() == source_id)
                && let Some(source) = seat.active_source.take()
            {
                source.proxy.destroy(context.conn);
                debug!(
                    seat = seat_name,
                    id = &source.item_id[..12],
                    "clipboard ownership ended"
                );
            }
        }
        _ => {}
    }
}

async fn send_payload(file: std::fs::File, fd: OwnedFd) -> Result<(), std::io::Error> {
    tokio::task::spawn_blocking(move || {
        let mut output = std::fs::File::from(fd);
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut offset = 0_u64;

        loop {
            let count = file.read_at(&mut buffer, offset)?;
            if count == 0 {
                return Ok(());
            }
            std::io::Write::write_all(&mut output, &buffer[..count])?;
            offset += count as u64;
        }
    })
    .await
    .map_err(std::io::Error::other)?
}

fn registry_event(
    connection: &mut Connection<State>,
    state: &mut State,
    event: &wl_registry::Event,
) {
    match event {
        wl_registry::Event::Global(global) if global.is::<WlSeat>() => {
            let seat_name = global.name;
            let seat = match global.bind(connection, 1..=9) {
                Ok(seat) => seat,
                Err(error) => {
                    warn!(seat = seat_name, %error, "failed to bind Wayland seat");
                    return;
                }
            };
            let device = state
                .manager
                .get_data_device_with_cb(connection, seat, move |context| {
                    device_event(seat_name, context)
                });
            state.seats.insert(
                seat_name,
                Seat {
                    seat,
                    device,
                    offers: HashMap::new(),
                    active_source: None,
                    ignore_next_selection: false,
                },
            );
            debug!(seat = seat_name, "bound Wayland seat");
        }
        wl_registry::Event::Global(_) => {}
        wl_registry::Event::GlobalRemove(name) => {
            if let Some(seat) = state.seats.remove(name) {
                destroy_seat(connection, seat);
                debug!(seat = name, "removed Wayland seat");
            }
        }
    }
}

fn device_event(seat_name: u32, context: EventCtx<State, ZwlrDataControlDeviceV1>) {
    let Some(seat) = context.state.seats.get_mut(&seat_name) else {
        warn!(seat = seat_name, "received event for unknown Wayland seat");
        return;
    };

    match context.event {
        zwlr_data_control_device_v1::Event::DataOffer(proxy) => {
            seat.offers.insert(
                proxy.id(),
                Offer {
                    proxy,
                    mime_types: Vec::new(),
                },
            );
            context.conn.set_callback_for(proxy, move |context| {
                offer_event(seat_name, proxy.id(), context);
            });
        }
        zwlr_data_control_device_v1::Event::Selection(Some(id)) => {
            if let Some(offer) = seat.offers.remove(&id) {
                if seat.ignore_next_selection {
                    seat.ignore_next_selection = false;
                    offer.proxy.destroy(context.conn);
                    debug!(seat = seat_name, "ignored Rift's own clipboard offer");
                    return;
                }
                context
                    .state
                    .selections
                    .push_back(Selection::Offer { seat_name, offer });
            } else {
                warn!(
                    seat = seat_name,
                    offer = id.as_u32(),
                    "selection referenced an unknown offer"
                );
            }
        }
        zwlr_data_control_device_v1::Event::Selection(None) => {
            context
                .state
                .selections
                .push_back(Selection::Clear { seat_name });
        }
        zwlr_data_control_device_v1::Event::PrimarySelection(id) => {
            if let Some(id) = id
                && let Some(offer) = seat.offers.remove(&id)
            {
                offer.proxy.destroy(context.conn);
            }
        }
        zwlr_data_control_device_v1::Event::Finished => {
            if let Some(seat) = context.state.seats.remove(&seat_name) {
                destroy_seat(context.conn, seat);
            }
        }
        _ => {}
    }
}

fn offer_event(
    seat_name: u32,
    offer_id: ObjectId,
    context: EventCtx<State, ZwlrDataControlOfferV1>,
) {
    let zwlr_data_control_offer_v1::Event::Offer(mime) = context.event else {
        return;
    };
    let Some(seat) = context.state.seats.get_mut(&seat_name) else {
        return;
    };
    let Some(offer) = seat.offers.get_mut(&offer_id) else {
        return;
    };
    let mime = mime.to_string_lossy().into_owned();
    if !offer.mime_types.contains(&mime) {
        offer.mime_types.push(mime);
    }
}

fn destroy_seat(connection: &mut Connection<State>, mut seat: Seat) {
    if let Some(source) = seat.active_source.take() {
        source.proxy.destroy(connection);
    }
    for offer in seat.offers.into_values() {
        offer.proxy.destroy(connection);
    }
    seat.device.destroy(connection);
    if seat.seat.version() >= 5 {
        seat.seat.release(connection);
    }
}

#[derive(Debug, thiserror::Error)]
enum CaptureError {
    #[error("clipboard data for {0:?} stalled")]
    Timeout(String),
    #[error("clipboard item exceeded the {0}-byte limit")]
    TooLarge(u64),
    #[error("clipboard MIME type contains a NUL byte")]
    InvalidMime(#[from] std::ffi::NulError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum WaylandError {
    #[error("failed to connect to Wayland: {0}")]
    Connect(#[from] wayrs_client::ConnectError),
    #[error("Wayland I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("compositor does not expose wlr-data-control: {0}")]
    Bind(#[from] BindError),
    #[error("clipboard storage failed: {0}")]
    Storage(#[from] crate::storage::StorageError),
    #[error("clipboard MIME type contains a NUL byte")]
    InvalidMime(#[from] std::ffi::NulError),
    #[error(transparent)]
    RuntimeDir(#[from] crate::config::RuntimeDirError),
    #[error("another Rift daemon is already running")]
    AlreadyRunning,
}

#[cfg(test)]
mod tests {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    use tokio::io::AsyncReadExt;

    use super::send_payload;

    async fn serve_once(file: std::fs::File) -> Vec<u8> {
        let (mut reader, writer) = tokio_pipe::pipe().unwrap();
        let fd = unsafe { OwnedFd::from_raw_fd(writer.into_raw_fd()) };
        let send = send_payload(file, fd);
        let receive = async {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await.unwrap();
            bytes
        };
        let (send_result, bytes) = tokio::join!(send, receive);
        send_result.unwrap();
        bytes
    }

    #[tokio::test]
    async fn serves_payload_repeatedly_from_shared_file_handle() {
        let mut temporary = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut temporary, b"grouped payload").unwrap();

        let first = serve_once(temporary.try_clone().unwrap()).await;
        let second = serve_once(temporary.try_clone().unwrap()).await;
        let concurrent = tokio::join!(
            serve_once(temporary.try_clone().unwrap()),
            serve_once(temporary.try_clone().unwrap()),
        );

        assert_eq!(first, b"grouped payload");
        assert_eq!(second, b"grouped payload");
        assert_eq!(concurrent.0, b"grouped payload");
        assert_eq!(concurrent.1, b"grouped payload");
    }
}
