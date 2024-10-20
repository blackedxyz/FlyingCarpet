#[cfg_attr(target_os = "linux", path = "linux/network.rs")]
#[cfg_attr(target_os = "windows", path = "windows/network.rs")]
pub mod network;

#[cfg_attr(target_os = "linux", path = "linux/bluetooth.rs")]
#[cfg_attr(target_os = "windows", path = "windows/bluetooth.rs")]
pub mod bluetooth;

mod receiving;
mod sending;
pub mod utils;

use bluetooth::{Bluetooth, BluetoothMessage};
use network::is_hosting;
use std::{
    error::Error,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use utils::get_key_and_ssid;

const CHUNKSIZE: usize = 1_000_000; // 1 MB
const MAJOR_VERSION: u64 = 8;

pub trait UI: Clone + Send + 'static {
    fn output(&self, msg: &str);
    fn show_progress_bar(&self);
    fn update_progress_bar(&self, percent: u8);
    fn enable_ui(&self);
    fn show_pin(&self, pin: &str);
}

#[derive(Clone)]
pub enum Mode {
    Send(Vec<PathBuf>),
    Receive(PathBuf),
}

#[derive(Clone, Copy)]
pub enum Peer {
    Android,
    IOS,
    Linux,
    MacOS,
    Windows,
}

impl From<&str> for Peer {
    fn from(peer: &str) -> Self {
        match peer {
            "android" => Peer::Android,
            "ios" => Peer::IOS,
            "linux" => Peer::Linux,
            "mac" => Peer::MacOS,
            "windows" => Peer::Windows,
            other => panic!("Bad peer: {}", other),
        }
    }
}

pub enum PeerResource {
    WifiClient(String), // used if joining, .0 is ip of gateway/peer/host
    WindowsHotspot(network::WindowsHotspot),
    LinuxHotspot,
}

// first String is the interface's name, second String is a base-10 representation of the u128 representation of the GUID of the interface. GUID is only used on Windows.
#[derive(serde::Deserialize, serde::Serialize)]
pub struct WiFiInterface(pub String, pub String);

pub struct Transfer {
    pub cancel_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub hotspot: Arc<Mutex<Option<PeerResource>>>,
    pub ssid: Arc<Mutex<Option<String>>>,
    pub ble_ui_tx: Mutex<Option<mpsc::Sender<bool>>>, // used by javascript to report user's choice about whether to pair with bluetooth device to windows custom pairing callback.
}

impl Transfer {
    pub fn new() -> Self {
        Transfer {
            cancel_handle: Mutex::new(None),
            hotspot: Arc::new(Mutex::new(None)),
            ssid: Arc::new(Mutex::new(None)),
            ble_ui_tx: Mutex::new(None),
        }
    }
}

pub async fn start_transfer<T: UI>(
    mode: String,
    using_bluetooth: bool,
    mut peer: Option<String>,
    mut password: Option<String>,
    interface: WiFiInterface,
    file_list: Option<Vec<String>>,
    receive_dir: Option<String>,
    ui: &T,
    hotspot: Arc<Mutex<Option<PeerResource>>>,
    state_ssid: Arc<Mutex<Option<String>>>,
    ble_ui_rx: mpsc::Receiver<bool>,
) -> Option<TcpStream> {
    // get files or receive directory
    let mode = if mode == "send" {
        let paths = file_list
            .expect("Send mode selected but no files present.")
            .iter()
            .map(|p| PathBuf::from_str(p).expect("Bad filename string."))
            .collect();
        Mode::Send(paths)
    } else if mode == "receive" {
        let folder = receive_dir.expect("Receive mode selected but no folder present.");
        Mode::Receive(PathBuf::from_str(&folder).expect("Bad folder string"))
    } else {
        panic!("Bad mode: {}", mode);
    };

    // if bluetooth, make that connection here first
    // for windows and linux, the central/client api can read and write synchronously, and we always know the ssid before starting hotspot, so we can just do that here before connecting to peer?
    // for servers/peripherals, does it matter? callbacks in both cases?

    if using_bluetooth {
        match negotiate_bluetooth(&mode, &password, ble_ui_rx, ui).await {
            Ok((p, ssid, pw)) => {
                peer = Some(p);
                if password.is_none() {
                    password = Some(pw);
                }
            }
            Err(e) => {
                ui.output(&format!("Could not establish Bluetooth connection: {}", e));
                return None;
            }
        }
    }

    let peer = Peer::from(
        peer.expect("Neither UI nor Bluetooth peer present.")
            .as_str(),
    );
    let password = password.unwrap(); // TODO

    let (key, ssid) = get_key_and_ssid(&password);

    {
        let mut _state_ssid = state_ssid.lock().expect("Couldn't lock state_ssid");
        *_state_ssid = Some(ssid.clone());
    }

    // start hotspot or connect to peer's
    let peer_resource =
        match network::connect_to_peer(peer, mode.clone(), ssid, password, interface, ui).await {
            Ok(p) => p,
            Err(e) => {
                ui.output(&format!("Error connecting to peer: {}", e));
                return None;
            }
        };

    tokio::task::yield_now().await;

    // start tcp connection
    let mut stream = match start_tcp(&peer_resource, ui).await {
        Ok(s) => s,
        Err(e) => {
            ui.output(&format!("Error starting TCP connection: {}", e));
            return None;
        }
    };

    // make sure the versions are compatible
    match confirm_version(&peer_resource, &mut stream).await {
        Ok(()) => (),
        Err(e) => {
            ui.output(&format!("Error confirming version: {}", e));
            return Some(stream);
        }
    };

    // confirm that one end is sending and the other is receiving
    match confirm_mode(mode.clone(), &peer_resource, &mut stream).await {
        Ok(()) => (),
        Err(e) => {
            ui.output(&format!("Error confirming mode: {}", e));
            return Some(stream);
        }
    };

    // store the hotspot in tauri's state
    // has to be in its own block here or tokio complains that this "mutex guard" is held across an await... who knows
    {
        let mut hotspot_value = hotspot.lock().expect("Couldn't lock hotspot mutex");
        *hotspot_value = Some(peer_resource);
    }

    match mode {
        Mode::Send(files) => {
            // tell receiving end how many files we're sending
            match stream.write_u64(files.len() as u64).await {
                Ok(()) => (),
                Err(e) => {
                    ui.output(&format!("Error writing number of files: {}", e));
                    return Some(stream);
                }
            }
            // find folder common to all files
            let mut common_folder = files[0].parent().or(Some(Path::new(""))).unwrap();
            if files.len() > 1 {
                for file in &files[1..] {
                    let current = file.parent().or(Some(Path::new(""))).unwrap();
                    let current_len = current.components().collect::<Vec<_>>().len();
                    let common_len = common_folder.components().collect::<Vec<_>>().len();
                    if current_len < common_len {
                        common_folder = current;
                    } else if current_len == common_len {
                        common_folder = current.parent().or(Some(Path::new(""))).unwrap();
                    }
                }
            }
            // send files
            for (i, file) in files.iter().enumerate() {
                let file_name = file
                    .file_name()
                    .expect("could not get filename from PathBuf")
                    .to_string_lossy();
                ui.output("=========================");
                ui.output(&format!(
                    "Sending file {} of {}. Filename: {}",
                    i + 1,
                    files.len(),
                    file_name
                ));
                match sending::send_file(file, common_folder, &key, &mut stream, ui).await {
                    Ok(_) => (),
                    Err(e) => {
                        ui.output(&format!("Error sending file: {}", e));
                        return Some(stream);
                    }
                };
            }
        }
        Mode::Receive(folder) => {
            // find out how many files we're receiving
            let num_files = match stream.read_u64().await {
                Ok(num) => num,
                Err(e) => {
                    ui.output(&format!("Error reading number of files: {}", e));
                    return Some(stream);
                }
            };
            // receive files
            for i in 0..num_files {
                ui.output("=========================");
                ui.output(&format!("Receiving file {} of {}.", i + 1, num_files,));
                let last_file = i == num_files - 1;
                match receiving::receive_file(&folder, &key, &mut stream, ui, last_file).await {
                    Ok(_) => (),
                    Err(e) => {
                        ui.output(&format!("Error receiving file: {}", e));
                        return Some(stream);
                    }
                }
            }
        }
    }

    ui.output("=========================");
    ui.output("Transfer complete");
    Some(stream)
}

pub async fn clean_up_transfer<T: UI>(
    stream: Option<TcpStream>,
    hotspot: Arc<Mutex<Option<PeerResource>>>,
    ssid: Arc<Mutex<Option<String>>>,
    ui: &T,
) {
    // shut down tcp stream
    match stream {
        Some(mut s) => {
            if s.shutdown().await.is_err() {
                ui.output("Failed to shut down TCP stream.")
            };
        }
        None => (),
    }
    // shut down hotspot
    shut_down_hotspot(&hotspot, &ssid, ui);
    // make sure hotspot gets dropped
    let mut hotspot_value = hotspot.lock().expect("Couldn't lock hotspot mutex");
    *hotspot_value = None;
    // enable UI
    ui.enable_ui();
}

fn shut_down_hotspot<T: UI>(
    hotspot: &Arc<Mutex<Option<PeerResource>>>,
    ssid: &Arc<Mutex<Option<String>>>,
    ui: &T,
) {
    let peer_resource = hotspot.lock().expect("Couldn't lock hotspot mutex.");
    let peer_resource = peer_resource.as_ref();
    let ssid = ssid.lock().expect("Couldn't lock SSID mutex.");
    match network::stop_hotspot(peer_resource, ssid.as_deref()) {
        Err(e) => ui.output(&format!("Error stopping hotspot: {}", e)),
        _ => (),
    };
}

async fn start_tcp<T: UI>(
    peer_resource: &PeerResource,
    ui: &T,
) -> Result<TcpStream, Box<dyn Error>> {
    let stream;
    match peer_resource {
        PeerResource::WifiClient(gateway) => {
            let addr = format!("{}:3290", gateway).parse::<SocketAddr>()?;
            stream = TcpStream::connect(addr).await?;
        }
        _ => {
            // linux or windows hotspot
            let addr = "0.0.0.0:3290".parse::<SocketAddr>()?;
            let listener = TcpListener::bind(&addr).await?;
            ui.output("Waiting for connection...");
            let (_stream, _socket_addr) = listener.accept().await?;
            ui.output("Connection accepted");
            stream = _stream;
        }
    }
    Ok(stream)
}

async fn confirm_mode(
    mode: Mode,
    peer_resource: &PeerResource,
    stream: &mut TcpStream,
) -> Result<(), Box<dyn Error>> {
    let our_mode = match mode {
        Mode::Send(..) => 1,
        Mode::Receive(..) => 0,
    };

    match peer_resource {
        PeerResource::WifiClient(..) => {
            // tell host what mode we selected and wait for confirmation that they don't match
            match mode {
                Mode::Send(_) => stream.write_u64(1).await?,
                Mode::Receive(_) => stream.write_u64(0).await?,
            };
            // wait to ensure host responds that mode selection was correct
            if stream.read_u64().await? != 1 {
                let msg = format!(
                    "Both ends of the transfer selected {}",
                    if our_mode == 0 { "receive" } else { "send" }
                );
                Err(msg)?
            }
        }
        // TODO: LinuxHotspot needed at all?
        PeerResource::WindowsHotspot(_) | PeerResource::LinuxHotspot => {
            // wait for guest to say what mode they selected, compare to our own, and report back
            let peer_mode = stream.read_u64().await?;
            if peer_mode == our_mode {
                let msg = format!(
                    "Both ends of the transfer selected {}",
                    if our_mode == 0 { "receive" } else { "send" }
                );
                // write failure to guest
                stream.write_u64(0).await?;
                Err(msg)?
            } else {
                // write success to guest
                stream.write_u64(1).await?;
            }
        }
    }
    Ok(())
}

async fn confirm_version(
    peer_resource: &PeerResource,
    stream: &mut TcpStream,
) -> Result<(), Box<dyn Error>> {
    // only really have to worry about version 6 as that's the only one online and in app store. it will do mode confirmation first,
    // and obey hotspot host/guest rule, and it will write 0 or 1 for mode, so we shouldn't deadlock with both ends waiting.
    let peer_version = match peer_resource {
        PeerResource::WifiClient(..) => {
            // send version to hotspot host
            stream.write_u64(MAJOR_VERSION).await?;
            // receive version of host
            stream.read_u64().await?
        }
        _ => {
            // wait for guest to say what version they're using, then send our version
            let _peer_version = stream.read_u64().await?;
            stream.write_u64(MAJOR_VERSION).await?;
            _peer_version
        }
    };

    if peer_version < MAJOR_VERSION {
        // we make decision
        if utils::is_compatible(peer_version) {
            stream.write_u64(1).await?; // report that versions are compatible
        } else {
            stream.write_u64(0).await?;
            Err(format!("Peer's version {} not compatible, please update Flying Carpet to the latest version on both devices.", peer_version))?;
        }
    } else if peer_version > MAJOR_VERSION {
        // peer makes decision
        if stream.read_u64().await? == 0 {
            Err(format!("Peer's version {} not compatible, please update Flying Carpet to the latest version on both devices.", peer_version))?;
        }
    } // otherwise, versions match, implicitly compatible
    Ok(())
}

async fn negotiate_bluetooth<T: UI>(
    mode: &Mode,
    password: &Option<String>,
    ble_ui_rx: mpsc::Receiver<bool>,
    ui: &T,
) -> Result<(String, String, String), Box<dyn Error>> {
    let (tx, mut rx) = mpsc::channel(1);
    let mut bluetooth = Bluetooth::new(tx)?;
    if let Mode::Send(_) = mode {
        ui.output("Advertising Bluetooth service...");
        bluetooth.peripheral.start_advertising()?;
        // TODO: can we block here until we have os/ssid/password? same as central, receive on channels?
        // if we're hosting, we will be receiving reads of our info: just pass references into start_advertising()?

        // let mut peer_mode = String::new();
        let mut peer_os = String::new();
        let mut peer_ssid = String::new();
        let mut peer_password = String::new();

        // TODO: do we need to wait to be notified that we've paired here, or just wait till central reads OS? don't think we get notification that central has paired with us in linux.
        // wait to pair
        // let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
        // if let BluetoothMessage::PeerOS(os) = msg {
        //     peer_os = os;
        // } else {
        //     // TODO: this isn't right because other types of message can come back?
        //     Err(format!(
        //         "Peripheral received incorrect BluetoothMessage: {:?}",
        //         msg
        //     ))?;
        // }

        // TODO: ensure that peer is using opposite mode? no. we wouldn't have paired if we had the same mode.
        // have to test what happens if we were already paired.

        // ensure we started advertising
        let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
        if msg != BluetoothMessage::StartedAdvertising {
            Err(format!("Peripheral received incorrect BluetoothMessage. Expected StartedAdvertising, got {:?}", msg))?;
        }

        // get OS of peer
        let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
        if let BluetoothMessage::PeerOS(os) = msg {
            peer_os = os;
        } else {
            Err(format!(
                "Peripheral received incorrect BluetoothMessage. Expected peer OS, got {:?}",
                msg
            ))?;
        }

        // TODO: if hosting, we need to wait for reads to happen to know that peer received messages and when to start transfer?
        if is_hosting(&Peer::from(peer_os.as_str()), mode) {
            let (_, ssid) =
                get_key_and_ssid(password.as_ref().expect("Hosting but do not have password"));
            Ok((
                peer_os,
                ssid.clone(),
                password.clone().expect("Hosting but do not have password"),
            ))
        } else {
            // if joining, receive writes
            // receive ssid
            let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
            if let BluetoothMessage::SSID(ssid) = msg {
                peer_ssid = ssid;
            } else {
                Err(format!(
                    "Peripheral received incorrect BluetoothMessage. Expected SSID, got {:?}",
                    msg
                ))?;
            }
            // receive password
            let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
            if let BluetoothMessage::Password(password) = msg {
                peer_password = password;
            } else {
                Err(format!(
                    "Peripheral received incorrect BluetoothMessage. Expected password, got {:?}",
                    msg
                ))?;
            }
            Ok((peer_os, peer_ssid, peer_password))
        }
    } else {
        // scan for device advertising flying carpet service
        ui.output("Scanning for Bluetooth peripherals...");
        bluetooth.central.scan(ble_ui_rx)?;

        // wait for result of scan. if PIN was shown, wait again for success or failure.
        println!("waiting for callback...");
        let msg = process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
        if let BluetoothMessage::Pin(_) = msg {
            process_bluetooth_message(&mut rx, &bluetooth, ui).await?;
        }

        // discover service and characteristics once paired
        bluetooth.central.get_services_and_characteristics().await?;

        // read peer's OS
        let peer = bluetooth
            .central
            .read(bluetooth::OS_CHARACTERISTIC_UUID)
            .await?;
        ui.output(&format!("Peer OS: {:?}", peer));

        // write OS
        bluetooth
            .central
            .write(bluetooth::OS_CHARACTERISTIC_UUID, bluetooth::OS)
            .await?;

        // read or write ssid and password
        let (ssid, password) = if network::is_hosting(&Peer::from(peer.as_str()), mode) {
            let password = password.clone().expect("Hosting but do not have password");
            let (_, ssid) = get_key_and_ssid(&password);
            bluetooth
                .central
                .write(bluetooth::SSID_CHARACTERISTIC_UUID, &ssid)
                .await?;
            bluetooth
                .central
                .write(bluetooth::PASSWORD_CHARACTERISTIC_UUID, &password)
                .await?;
            (ssid, password)
        } else {
            let ssid = bluetooth
                .central
                .read(bluetooth::SSID_CHARACTERISTIC_UUID)
                .await?;
            let password = bluetooth
                .central
                .read(bluetooth::PASSWORD_CHARACTERISTIC_UUID)
                .await?;
            (ssid, password)
        };
        Ok((peer, ssid, password))
    }
}

async fn process_bluetooth_message<T: UI>(
    rx: &mut mpsc::Receiver<BluetoothMessage>,
    bluetooth: &Bluetooth,
    ui: &T,
) -> Result<BluetoothMessage, Box<dyn Error>> {
    let msg = rx
        .recv()
        .await
        .expect("Bluetooth message channel unexpectedly closed.");
    println!("received {:?}", msg);
    bluetooth.central.stop_watching()?;
    println!("stopped watching");
    match msg {
        BluetoothMessage::Pin(ref pin) => {
            ui.show_pin(pin);
        }
        BluetoothMessage::PairSuccess => {
            // can use this to represent AlreadyPaired on windows? don't need to emit pin, just need to proceed.
            // and nothing will be blocked in central because the pairing_handler won't be called.
            ui.output("Successfully paired");
        }
        BluetoothMessage::PairFailure => Err("Pairing failed.")?,
        BluetoothMessage::AlreadyPaired => {
            ui.output("Already BLE paired with Bluetooth device");
        }
        BluetoothMessage::UserCanceled => Err("User canceled.")?,
        BluetoothMessage::StartedAdvertising => ui.output("Started advertising Bluetooth service"),
        BluetoothMessage::PeerOS(ref os) => ui.output(&format!("Peer's OS is {}", os)),
        BluetoothMessage::SSID(ref ssid) => ui.output(&format!("Peer's SSID is {}", ssid)),
        BluetoothMessage::Password(ref password) => {
            ui.output(&format!("Peer's password is {}", password))
        }
        BluetoothMessage::Other(ref s) => ui.output(&format!("Bluetooth peering result: {}", s)),
    };
    Ok(msg)
}

// TODO:
// test multiple transfers back to back, windows central unpaired but ios peripheral still paired
// why is ios looking for ip address for a long time?
// test switching os...
// "send mode selected but no files present"
// hide os buttons when using bluetooth
// how did windows read OS "windows" from itself when acting as central but not peripheral?
// does linux need any channels for bluetooth?
// folder send check box? or just rely on drag and drop? if so, disable it, store/restore on refresh.
// fix tests
// fix bug where multiple start/cancel clicks stack while waiting for transfer to cancel, at least on linux: have to get whatever is blocking on background thread?
// update screenshots, version bump
// show qr code after refresh
// test pulling wifi card, quitting program, etc.

// LATER MAYBE:
// code signing for windows?
// faster?
// cli version?
// move expand_files into utils and make tauri's version a wrapper for CLI version
// hosted network stuff on windows?
// send folder mode?
// recreate directory structure if all submitted files are in same dir. taken for granted in gui? only problem for cli? not if dropping appends... only allow when using send-folder?
// remove file selection box and replace start button with Choose Files/Choose Folder? gets in the way of drag and drop... so no?
// optional password length?
// move password length constant into rust, fetch in javascript
