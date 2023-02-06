use azure_mgmt_compute::Client;
use leb128;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    select,
    spawn,
    sync::broadcast::{self, Sender},
    time::{sleep, sleep_until, Duration, Instant},
};

use super::Args;

#[derive(Debug, Clone, PartialEq)]
enum ProxyState {
    Starting,
    Running,
    StopPending,
    Stopping,
    Stopped,
}

#[derive(Debug, Clone, PartialEq)]
enum VMStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
}

struct State {
    // Number of clients connected to the server
    ref_count: u64,

    // Whether we are in the timeout before stopping
    status: ProxyState,
}

struct Proxy {
    // Ip of the remote server
    server_ip: String,

    // Timeout before stopping the server, in seconds
    timeout: f64,

    // Server state values, locked behind a mutex
    state: Mutex<State>,

    // Notify state changes
    state_notifier: Sender<ProxyState>,

    // Azure client
    azure_client: Client,

    ping_response: Vec<u8>,

    // All arguments
    args: Args,
}

impl Proxy {
    fn new(args: Args, azure_client: Client) -> Arc<Self> {
        let message = "{\"version\":{\"name\":\"1.19.3\",\"protocol\":761},\"players\":{\"max\":0,\"online\":0},\"description\":{\"text\":\"Server is starting...\"}}";
        let message_buf = message.as_bytes();
        let mut message_length_buf: Vec<u8> = vec![0; 0];
        let mut packet_length_buf: Vec<u8> = vec![0; 0];
        leb128::write::signed(&mut message_length_buf, message_buf.len() as i64).unwrap();
        leb128::write::signed(&mut packet_length_buf, (message_buf.len() + message_length_buf.len() + 1) as i64).unwrap();

        let buffer = packet_length_buf.iter().chain([0x00].iter()).chain(message_length_buf.iter()).chain(message_buf.iter()).map(|&x| x).collect::<Vec<u8>>();

        let (state_notifier, _) = broadcast::channel::<ProxyState>(1);
        Arc::new(Self {
            server_ip: args.server.clone(),
            timeout: args.timeout,
            state: Mutex::new(State {
                ref_count: 0,
                status: ProxyState::Stopped,
            }),
            state_notifier,
            azure_client,
            ping_response: buffer,
            args,
        })
    }

    fn update_status_locked(self: &Arc<Self>, state: &mut MutexGuard<State>, new_status: ProxyState) {
        state.status = new_status.clone();
        self.state_notifier.send(new_status).unwrap();
    }


    fn update_status(self: &Arc<Self>, new_status: ProxyState) {
        let mut state = self.state.lock().unwrap();
        self.update_status_locked(&mut state, new_status);
    }

    async fn start_vm(self: &Arc<Self>) -> bool {
        if self.get_vm_status().await == Some(VMStatus::Running) {
            return true;
        }

        let response = self.azure_client.virtual_machines_client().start(
            self.args.azure_resource_group.clone(),
            self.args.azure_vm_name.clone(),
            self.args.azure_subscription_id.clone(),
        ).send().await;

        match response {
            Ok(_) => {
                self.wait_for_vm_status(VMStatus::Running).await;
                true
            },
            Err(e) => {
                eprintln!("Error starting VM: {:?}", e);
                false
            }
        }
    }

    async fn stop_vm(self: &Arc<Self>) -> bool {
        if self.get_vm_status().await == Some(VMStatus::Stopped) {
            return true;
        }

        let response = self.azure_client.virtual_machines_client().deallocate(
            self.args.azure_resource_group.clone(),
            self.args.azure_vm_name.clone(),
            self.args.azure_subscription_id.clone(),
        ).send().await;

        match response {
            Ok(_) => {
                self.wait_for_vm_status(VMStatus::Stopped).await;
                true
            },
            Err(e) => {
                eprintln!("Error stopping VM: {:?}", e);
                false
            }
        }
    }

    async fn get_vm_status(self: &Arc<Self>) -> Option<VMStatus> {
        let statuses = self.azure_client
            .virtual_machines_client()
            .get(
                self.args.azure_resource_group.clone(),
                self.args.azure_vm_name.clone(),
                self.args.azure_subscription_id.clone()
            )
            .expand("instanceView")
            .into_future()
            .await.ok()?
            .properties?
            .instance_view?
            .statuses;

        statuses.into_iter().find_map(|status| {
            let code = status.code?.clone();
            let code = code.strip_prefix("PowerState/")?;

            match code {
                "starting" => Some(VMStatus::Starting),
                "running" => Some(VMStatus::Running),
                "deallocating" => Some(VMStatus::Stopping),
                "deallocated" => Some(VMStatus::Stopped),
                "stopped" => Some(VMStatus::Stopped),
                _ => {
                    eprintln!("Unknown VM status: {}", code);
                    None
                }
            }
        })
    }

    async fn wait_for_vm_status(self: &Arc<Self>, expected_status: VMStatus) {
        loop {
            let vm_status = self.get_vm_status().await;
            if vm_status == Some(expected_status.clone()) {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn start(self: &Arc<Self>) -> io::Result<()> {
        // Listen for connections from the client and forward to the
        // server.
        let client_listener_ip = self.args.client.as_str();
        let listener = TcpListener::bind(client_listener_ip).await?;
        println!("Listening on {}", client_listener_ip);

        loop {
            let (client, _) = listener.accept().await?;

            let proxy = self.clone();
            spawn(async move {
                proxy.process_client(client).await;
            });
        }
    }

    async fn process_client(self: &Arc<Self>, client: TcpStream) {
        let client_ip = client.peer_addr().unwrap().to_string();
        let status;

        {
            let state = self.state.lock().unwrap();
            status = state.status.clone();
        }

        let (mut cread, mut cwrite) = client.into_split();

        // If the server is not running, check for a minecraft status request and
        // respond with a loading message.
        if status != ProxyState::Running && status != ProxyState::StopPending {
            let mut buf = [0; 1024];
            match cread.peek(&mut buf).await {
                Ok(0) => (),
                Ok(n) => {
                    if n >= 2 && buf[0] == 0x10 && buf[1] == 0x00 {
                        cread.read(&mut buf[..n]).await.unwrap();
                        cwrite.write_all(&self.ping_response).await.ok();
                        cwrite.flush().await.ok();
                        println!("Sent status");
                    } else if n >= 2 && buf[1] == 0x01 {
                        let mut outbuf = vec![0 as u8; 0];
                        cread.read(&mut buf[..n]).await.unwrap();

                        leb128::write::signed(&mut outbuf, (n as i64 - 2) + 1).unwrap();
                        cwrite.write_all([outbuf.as_slice(), &[0x01 as u8], &buf[2..n]].concat().as_slice()).await.ok();
                        cwrite.flush().await.ok();

                        let mut client = cread.reunite(cwrite).unwrap();
                        client.shutdown().await.ok();
                        return;
                    }
                }
                Err(e) => {
                    eprintln!("Error reading from client {}: {:?}", client_ip, e);
                }
            }
        }

        self.start_server(client_ip.as_str()).await;

        let server = TcpStream::connect(self.server_ip.as_str()).await;
        if server.is_err() {
            eprintln!("Failed to connect to origin server");
            return;
        }

        let (mut sread, mut swrite) = server.unwrap().into_split();

        /*
        let c2s = spawn(async move {
            let mut buf = [0; 1024];
            loop {
                match cread.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => {
                        print!("{:02X?}", &buf[..n]);
                        if swrite.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        let s2c = spawn(async move {
            let mut buf = [0; 1024];
            loop {
                match sread.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => {
                        print!("{:02X?}", &buf[..n]);
                        if cwrite.write_all(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        });
        */

        let c2s = spawn(async move { io::copy(&mut cread, &mut swrite).await });
        let s2c = spawn(async move { io::copy(&mut sread, &mut cwrite).await });

        // Wait for either the client or server to disconnect
        select!{
            _ = c2s => {},
            _ = s2c => {},
        }

        self.stop_server(client_ip.as_str()).await;
    }

    async fn stop_task(self: &Arc<Self>) {
        let mut state_receiver = self.state_notifier.subscribe();
        let mut previous_state = ProxyState::Stopped;
        loop {
            let new_state = state_receiver.recv().await.unwrap();
            let prev_state_copy = previous_state.clone();
            previous_state = new_state.clone();

            match (prev_state_copy, new_state) {
                (ProxyState::StopPending, ProxyState::Stopping) => {},
                (ProxyState::StopPending, ProxyState::Running) => {
                    println!("Stop cancelled");
                    continue;
                }
                _ => continue,
            }

            println!("Stopping server");
            self.stop_vm().await;
            println!("Server stopped");
            self.update_status(ProxyState::Stopped);
        }
    }

    async fn start_server(self: &Arc<Self>, client_ip: &str) {
        let power_state: ProxyState;
        {
            let mut state = self.state.lock().unwrap();
            state.ref_count += 1;
            println!("Client {} connected. Total clients: {}", client_ip, state.ref_count);
            if state.ref_count != 1 {
                return;
            }
            power_state = state.status.clone();

            // Do any state transitions first, while we still hold the lock
            let new_status = match power_state {
                ProxyState::StopPending => Some(ProxyState::Running),
                ProxyState::Stopped => Some(ProxyState::Starting),
                _ => None,
            };

            if let Some(new_status) = new_status {
                self.update_status_locked(&mut state, new_status);
            }
        }

        match power_state {
            ProxyState::Starting | ProxyState::Stopping => {
                let mut state_receiver = self.state_notifier.subscribe();
                println!("Waiting for vm to start");
                loop {
                    select!{
                        new_state = state_receiver.recv() => {
                            if new_state == Ok(ProxyState::Running) {
                                break;
                            }
                        },
                    }
                }
            }
            ProxyState::Stopped => {
                println!("Starting server");
                self.start_vm().await;
                println!("Server started");
                self.update_status(ProxyState::Running);
            }
            _ => (),
        };
    }

    async fn stop_server(self: &Arc<Self>, client_ip: &str) {
        {
            let mut state = self.state.lock().unwrap();
            state.ref_count -= 1;
            println!("Client {} disconnected. Total clients: {}", client_ip, state.ref_count);
            if state.ref_count != 0 {
                return
            }

            self.update_status_locked(&mut state, ProxyState::StopPending);
        }

        println!("No clients remaining. Stopping server in {} seconds", self.timeout);

        let proxy = self.clone();
        spawn(async move {
            let mut state_receiver = proxy.state_notifier.subscribe();
            let sleep_end = Instant::now() + Duration::from_secs_f64(proxy.timeout);
            loop {
                select!{biased;
                    new_state = state_receiver.recv() => {
                        match new_state.unwrap() {
                            ProxyState::Running | ProxyState::Stopping | ProxyState::Stopped => return,
                            _ => continue,
                        }
                    },
                    _ = sleep_until(sleep_end) => {
                        proxy.update_status(ProxyState::Stopping);
                        break;
                    }
                }
            }
        });
    }
}

pub async fn start_proxy(args: Args, azure_client: Client) -> io::Result<()> {
    let proxy = Proxy::new(args, azure_client);
    let proxy2 = proxy.clone();

    spawn(async move {
        proxy.stop_task().await
    });
    proxy2.start().await
}

