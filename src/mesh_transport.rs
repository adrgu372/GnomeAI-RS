//! Embedded Arti: one Tor client, separate onion service per relationship.
//! Only encrypted peer frames traverse these tunnels. Provider HTTP is unchanged.
use anyhow::{Context, Result, bail};
use arti_client::TorClient;
use safelog::DisplayRedacted;
use futures_util::StreamExt;
use sha2::{Digest,Sha256};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::{Arc,Mutex}, time::Duration};
use tokio::{io::{AsyncReadExt,AsyncWriteExt}, net::{TcpListener,TcpStream},sync::{mpsc,Semaphore}};
use tokio_util::{compat::FuturesAsyncReadCompatExt,sync::CancellationToken};

enum Command { Start { channel:String,port:u16 }, Revoke(String) }
pub struct Mesh { tx:mpsc::Sender<Command>, state:Arc<Mutex<Value>>, stop:CancellationToken }
impl Mesh {
    pub fn new(home:PathBuf)->Self {
        let (tx,rx)=mpsc::channel(32);
        let state=Arc::new(Mutex::new(json!({"status":"stopped","socks_port":0,"onions":{}})));
        let stop=CancellationToken::new();let child=stop.clone();let shared=state.clone();
        tokio::spawn(async move {
            tokio::select! { _=child.cancelled()=>{}, result=run(home,rx,shared.clone(),child.clone())=>{
                if let Err(error)=result {shared.lock().unwrap()["status"]=json!(format!("Tor failed: {error}"));}
            }}
        });
        Self {tx,state,stop}
    }
    pub fn request(&self,action:&str,payload:&Value)->Result<Value> {
        if action=="mesh_start" {
            let channel=payload["channel"].as_str().unwrap_or("");
            if !channel.is_empty() && (channel.len()!=43 || !channel.bytes().all(|b|b.is_ascii_alphanumeric()||b==b'-'||b==b'_')) {bail!("Invalid mesh channel");}
            let port=payload["port"].as_u64().filter(|p|*p>0 && *p<=65535).context("Local peer port required")? as u16;
            self.tx.try_send(Command::Start {channel:channel.into(),port}).context("Tor request queue is busy")?;
        } else if action=="mesh_revoke" {
            self.tx.try_send(Command::Revoke(payload["channel"].as_str().context("Channel required")?.into()))?;
        }
        Ok(self.state.lock().unwrap().clone())
    }
}
impl Drop for Mesh {fn drop(&mut self){self.stop.cancel();}}

async fn run(home:PathBuf,mut rx:mpsc::Receiver<Command>,state:Arc<Mutex<Value>>,stop:CancellationToken)->Result<()> {
    let Some(first)=rx.recv().await else{return Ok(());};
    state.lock().unwrap()["status"]=json!("Bootstrapping Tor…");
    let root=home.join("store/tor");
    std::fs::create_dir_all(&root)?;
    #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700))?;}
    let config=arti_client::config::TorClientConfigBuilder::from_directories(root.join("state"),root.join("cache")).build()?;
    let client=TorClient::create_bootstrapped(config).await?;
    let socks=TcpListener::bind((std::net::Ipv4Addr::LOCALHOST,0)).await?;
    state.lock().unwrap()["socks_port"]=json!(socks.local_addr()?.port());
    let budget=Arc::new(Semaphore::new(32));
    let dialer=client.clone();let proxy_stop=stop.clone();let slots=budget.clone();
    tokio::spawn(async move {
        loop {
            let incoming=tokio::select!{_=proxy_stop.cancelled()=>break,s=socks.accept()=>s};
            let Ok((mut local,_))=incoming else {break;};
            let Ok(permit)=slots.clone().try_acquire_owned() else {continue;};
            let client=dialer.isolated_client();let cancel=proxy_stop.clone();
            tokio::spawn(async move {let _permit=permit;
                let task=async {
                    let mut greeting=[0;3];local.read_exact(&mut greeting).await?;
                    if greeting!=[5,1,0]{bail!("SOCKS authentication method unsupported");}
                    local.write_all(&[5,0]).await?;
                    let mut header=[0;5];local.read_exact(&mut header).await?;
                    if header[..4]!=[5,1,0,3]{bail!("Only onion hostnames are accepted");}
                    let mut host=vec![0;header[4] as usize];local.read_exact(&mut host).await?;
                    let port=local.read_u16().await?;
                    let host=std::str::from_utf8(&host)?;
                    if !valid_onion(host) || port!=80 {bail!("Only GnomeAI onion endpoints are accepted");}
                    let remote=tokio::time::timeout(Duration::from_secs(120),client.connect((host,port))).await??;
                    local.write_all(&[5,0,0,1,127,0,0,1,0,0]).await?;
                    Ok::<_,anyhow::Error>(remote)
                };
                let setup=tokio::select!{_=cancel.cancelled()=>return,r=tokio::time::timeout(Duration::from_secs(130),task)=>r};
                if let Ok(Ok(remote))=setup {let mut remote=remote.compat();tokio::select!{_=cancel.cancelled()=>{},_=tokio::io::copy_bidirectional(&mut local,&mut remote)=>{}}}
            });
        }
    });
    let mut services=HashMap::new();let mut next=Some(first);
    state.lock().unwrap()["status"]=json!("Tor ready");
    loop {
        let cmd=if let Some(cmd)=next.take(){cmd}else{tokio::select!{_=stop.cancelled()=>break,cmd=rx.recv()=>{let Some(cmd)=cmd else{break;};cmd}}};
        match cmd {
            Command::Start{channel,port} => {
                if channel.is_empty() || services.contains_key(&channel){continue;}
                let fingerprint=format!("{:x}",Sha256::digest(channel.as_bytes()));
                let nickname=format!("p{}",&fingerprint[..60]);
                let config=tor_hsservice::OnionServiceConfig::builder().nickname(nickname.parse()?).build()?;
                let (service,requests)=client.launch_onion_service(config)?.context("Onion service disabled")?;
                let onion=service.onion_address().context("Onion identity unavailable")?.display_unredacted().to_string();
                state.lock().unwrap()["onions"][&channel]=json!(onion);
                let service_stop=stop.child_token();let cancel=service_stop.clone();let slots=budget.clone();
                tokio::spawn(async move {
                    let streams=tor_hsservice::handle_rend_requests(requests);tokio::pin!(streams);
                    loop {
                        let request=tokio::select!{_=cancel.cancelled()=>break,r=streams.next()=>{let Some(r)=r else{break;};r}};
                        let wanted=matches!(request.request(),tor_proto::stream::IncomingStreamRequest::Begin(begin) if begin.port()==80);
                        if !wanted {let _=request.reject(tor_cell::relaycell::msg::End::new_misc()).await;continue;}
                        let Ok(permit)=slots.clone().try_acquire_owned() else {continue;};
                        let cancel=cancel.clone();
                        tokio::spawn(async move {let _permit=permit;
                            let task=async {
                                let mut local=TcpStream::connect((std::net::Ipv4Addr::LOCALHOST,port)).await?;
                                let mut remote=request.accept(tor_cell::relaycell::msg::Connected::new_empty()).await?.compat();
                                tokio::io::copy_bidirectional(&mut local,&mut remote).await?;
                                Ok::<_,anyhow::Error>(())
                            };
                            tokio::select!{_=cancel.cancelled()=>{},_=task=>{}}
                        });
                    }
                    drop(service);
                });
                services.insert(channel,service_stop);
            }
            Command::Revoke(channel)=>{
                if let Some(token)=services.remove(&channel){token.cancel();}
                if let Some(onions)=state.lock().unwrap()["onions"].as_object_mut(){onions.remove(&channel);}
            }
        }
    }
    Ok(())
}
fn valid_onion(host:&str)->bool {host.len()==62 && host.ends_with(".onion") && host[..56].bytes().all(|b|b.is_ascii_lowercase()||(b'2'..=b'7').contains(&b))}
