use std::{fmt::Debug, pin::Pin, sync::Arc};

use async_trait::async_trait;
use dashmap::DashMap;
use easytier_rpc::TunnelInfo;
use futures::{stream::FuturesUnordered, SinkExt, StreamExt};
use rkyv::{Archive, Deserialize, Serialize};
use std::net::SocketAddr;
use tokio::{net::UdpSocket, sync::Mutex, task::JoinSet};
use tokio_util::{
    bytes::{Buf, Bytes, BytesMut},
    udp::UdpFramed,
};
use tracing::Instrument;

use crate::{
    common::rkyv_util::{self, encode_to_bytes},
    tunnels::{build_url_from_socket_addr, close_tunnel, TunnelConnCounter, TunnelConnector},
};

use super::{
    codec::BytesCodec,
    common::{setup_sokcet2, FramedTunnel, TunnelWithCustomInfo},
    ring_tunnel::create_ring_tunnel_pair,
    DatagramSink, DatagramStream, Tunnel, TunnelListener,
};

pub const UDP_DATA_MTU: usize = 2500;

#[derive(Archive, Deserialize, Serialize, Debug)]
#[archive(compare(PartialEq), check_bytes)]
// Derives can be passed through to the generated type:
#[archive_attr(derive(Debug))]
pub enum UdpPacketPayload {
    Syn,
    Sack,
    HolePunch(Vec<u8>),
    Data(Vec<u8>),
}

#[derive(Archive, Deserialize, Serialize, Debug)]
#[archive(compare(PartialEq), check_bytes)]
#[archive_attr(derive(Debug))]
pub struct UdpPacket {
    pub conn_id: u32,
    pub payload: UdpPacketPayload,
}

impl UdpPacket {
    pub fn new_data_packet(conn_id: u32, data: Vec<u8>) -> Self {
        Self {
            conn_id,
            payload: UdpPacketPayload::Data(data),
        }
    }

    pub fn new_hole_punch_packet(data: Vec<u8>) -> Self {
        Self {
            conn_id: 0,
            payload: UdpPacketPayload::HolePunch(data),
        }
    }

    pub fn new_syn_packet(conn_id: u32) -> Self {
        Self {
            conn_id,
            payload: UdpPacketPayload::Syn,
        }
    }

    pub fn new_sack_packet(conn_id: u32) -> Self {
        Self {
            conn_id,
            payload: UdpPacketPayload::Sack,
        }
    }
}

fn try_get_data_payload(mut buf: BytesMut, conn_id: u32) -> Option<BytesMut> {
    let Ok(udp_packet) = rkyv_util::decode_from_bytes_checked::<UdpPacket>(&buf) else {
        tracing::warn!(?buf, "udp decode error");
        return None;
    };

    if udp_packet.conn_id != conn_id.clone() {
        tracing::warn!(?udp_packet, ?conn_id, "udp conn id not match");
        return None;
    }

    let ArchivedUdpPacketPayload::Data(payload) = &udp_packet.payload else {
        tracing::warn!(?udp_packet, "udp payload not data");
        return None;
    };

    let ptr_range = payload.as_ptr_range();
    let offset = ptr_range.start as usize - buf.as_ptr() as usize;
    let len = ptr_range.end as usize - ptr_range.start as usize;
    buf.advance(offset);
    buf.truncate(len);
    tracing::trace!(?offset, ?len, ?buf, "udp payload data");

    Some(buf)
}

fn get_tunnel_from_socket(
    socket: Arc<UdpSocket>,
    addr: SocketAddr,
    conn_id: u32,
) -> Box<dyn super::Tunnel> {
    let udp = UdpFramed::new(socket.clone(), BytesCodec::new(UDP_DATA_MTU));
    let (sink, stream) = udp.split();

    let recv_addr = addr;
    let stream = stream.filter_map(move |v| async move {
        tracing::trace!(?v, "udp stream recv something");
        if v.is_err() {
            tracing::warn!(?v, "udp stream error");
            return Some(Err(super::TunnelError::CommonError(
                "udp stream error".to_owned(),
            )));
        }

        let (buf, addr) = v.unwrap();
        assert_eq!(addr, recv_addr.clone());
        Some(Ok(try_get_data_payload(buf, conn_id.clone())?))
    });
    let stream = Box::pin(stream);

    let sender_addr = addr;
    let sink = Box::pin(sink.with(move |v: Bytes| async move {
        if false {
            return Err(super::TunnelError::CommonError("udp sink error".to_owned()));
        }

        // TODO: two copy here, how to avoid?
        let udp_packet = UdpPacket::new_data_packet(conn_id, v.to_vec());
        tracing::trace!(?udp_packet, ?v, "udp send packet");
        let v = encode_to_bytes::<_, UDP_DATA_MTU>(&udp_packet);

        Ok((v, sender_addr))
    }));

    FramedTunnel::new_tunnel_with_info(
        stream,
        sink,
        // TODO: this remote addr is not a url
        super::TunnelInfo {
            tunnel_type: "udp".to_owned(),
            local_addr: super::build_url_from_socket_addr(
                &socket.local_addr().unwrap().to_string(),
                "udp",
            )
            .into(),
            remote_addr: super::build_url_from_socket_addr(&addr.to_string(), "udp").into(),
        },
    )
}

struct StreamSinkPair(
    Pin<Box<dyn DatagramStream>>,
    Pin<Box<dyn DatagramSink>>,
    u32,
);
type ArcStreamSinkPair = Arc<Mutex<StreamSinkPair>>;

pub struct UdpTunnelListener {
    addr: url::Url,
    socket: Option<Arc<UdpSocket>>,

    sock_map: Arc<DashMap<SocketAddr, ArcStreamSinkPair>>,
    forward_tasks: Arc<Mutex<JoinSet<()>>>,

    conn_recv: tokio::sync::mpsc::Receiver<Box<dyn Tunnel>>,
    conn_send: Option<tokio::sync::mpsc::Sender<Box<dyn Tunnel>>>,
}

impl UdpTunnelListener {
    pub fn new(addr: url::Url) -> Self {
        let (conn_send, conn_recv) = tokio::sync::mpsc::channel(100);
        Self {
            addr,
            socket: None,
            sock_map: Arc::new(DashMap::new()),
            forward_tasks: Arc::new(Mutex::new(JoinSet::new())),
            conn_recv,
            conn_send: Some(conn_send),
        }
    }

    async fn try_forward_packet(
        sock_map: &DashMap<SocketAddr, ArcStreamSinkPair>,
        buf: BytesMut,
        addr: SocketAddr,
    ) -> Result<(), super::TunnelError> {
        let entry = sock_map.get_mut(&addr);
        if entry.is_none() {
            log::warn!("udp forward packet: {:?}, {:?}, no entry", addr, buf);
            return Ok(());
        }

        log::trace!("udp forward packet: {:?}, {:?}", addr, buf);
        let entry = entry.unwrap();
        let pair = entry.value().clone();
        drop(entry);

        let Some(buf) = try_get_data_payload(buf, pair.lock().await.2) else {
            return Ok(());
        };
        pair.lock().await.1.send(buf.freeze()).await?;
        Ok(())
    }

    async fn handle_connect(
        socket: Arc<UdpSocket>,
        addr: SocketAddr,
        forward_tasks: Arc<Mutex<JoinSet<()>>>,
        sock_map: Arc<DashMap<SocketAddr, ArcStreamSinkPair>>,
        local_url: url::Url,
        conn_id: u32,
    ) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        tracing::info!(?conn_id, ?addr, "udp connection accept handling",);

        let udp_packet = UdpPacket::new_sack_packet(conn_id);
        let sack_buf = encode_to_bytes::<_, UDP_DATA_MTU>(&udp_packet);
        socket.send_to(&sack_buf, addr).await?;

        let (ctunnel, stunnel) = create_ring_tunnel_pair();
        let udp_tunnel = get_tunnel_from_socket(socket.clone(), addr, conn_id);
        let ss_pair = StreamSinkPair(ctunnel.pin_stream(), ctunnel.pin_sink(), conn_id);
        let addr_copy = addr.clone();
        sock_map.insert(addr, Arc::new(Mutex::new(ss_pair)));
        let ctunnel_stream = ctunnel.pin_stream();
        forward_tasks.lock().await.spawn(async move {
            let ret = ctunnel_stream
                .map(|v| {
                    tracing::trace!(?v, "udp stream recv something in forward task");
                    if v.is_err() {
                        return Err(super::TunnelError::CommonError(
                            "udp stream error".to_owned(),
                        ));
                    }
                    Ok(v.unwrap().freeze())
                })
                .forward(udp_tunnel.pin_sink())
                .await;
            if let None = sock_map.remove(&addr_copy) {
                log::warn!("udp forward packet: {:?}, no entry", addr_copy);
            }
            close_tunnel(&ctunnel).await.unwrap();
            log::warn!("udp connection forward done: {:?}, {:?}", addr_copy, ret);
        });

        Ok(Box::new(TunnelWithCustomInfo::new(
            stunnel,
            TunnelInfo {
                tunnel_type: "udp".to_owned(),
                local_addr: local_url.into(),
                remote_addr: build_url_from_socket_addr(&addr.to_string(), "udp").into(),
            },
        )))
    }

    pub fn get_socket(&self) -> Option<Arc<UdpSocket>> {
        self.socket.clone()
    }
}

#[async_trait]
impl TunnelListener for UdpTunnelListener {
    async fn listen(&mut self) -> Result<(), super::TunnelError> {
        let addr = super::check_scheme_and_get_socket_addr::<SocketAddr>(&self.addr, "udp")?;

        let socket2_socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        setup_sokcet2(&socket2_socket, &addr)?;
        self.socket = Some(Arc::new(UdpSocket::from_std(socket2_socket.into())?));

        let socket = self.socket.as_ref().unwrap().clone();
        let forward_tasks = self.forward_tasks.clone();
        let sock_map = self.sock_map.clone();
        let conn_send = self.conn_send.take().unwrap();
        let local_url = self.local_url().clone();
        self.forward_tasks.lock().await.spawn(
            async move {
                loop {
                    let mut buf = BytesMut::new();
                    buf.resize(2500, 0);
                    let (_size, addr) = socket.recv_from(&mut buf).await.unwrap();
                    let _ = buf.split_off(_size);
                    log::trace!(
                        "udp recv packet: {:?}, buf: {:?}, size: {}",
                        addr,
                        buf,
                        _size
                    );

                    let Ok(udp_packet) = rkyv_util::decode_from_bytes_checked::<UdpPacket>(&buf)
                    else {
                        tracing::warn!(?buf, "udp decode error in forward task");
                        continue;
                    };

                    if matches!(udp_packet.payload, ArchivedUdpPacketPayload::Syn) {
                        let conn = Self::handle_connect(
                            socket.clone(),
                            addr,
                            forward_tasks.clone(),
                            sock_map.clone(),
                            local_url.clone(),
                            udp_packet.conn_id.into(),
                        )
                        .await
                        .unwrap();
                        if let Err(e) = conn_send.send(conn).await {
                            tracing::warn!(?e, "udp send conn to accept channel error");
                        }
                    } else {
                        Self::try_forward_packet(sock_map.as_ref(), buf, addr)
                            .await
                            .unwrap();
                    }
                }
            }
            .instrument(tracing::info_span!("udp forward task", ?self.socket)),
        );

        // let forward_tasks_clone = self.forward_tasks.clone();
        // tokio::spawn(async move {
        //     loop {
        //         let mut locked_forward_tasks = forward_tasks_clone.lock().await;
        //         tokio::select! {
        //             ret = locked_forward_tasks.join_next() => {
        //                 tracing::warn!(?ret, "udp forward task exit");
        //             }
        //             else => {
        //                 drop(locked_forward_tasks);
        //                 tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        //                 continue;
        //             }
        //         }
        //     }
        // });

        Ok(())
    }

    async fn accept(&mut self) -> Result<Box<dyn super::Tunnel>, super::TunnelError> {
        log::info!("start udp accept: {:?}", self.addr);
        while let Some(conn) = self.conn_recv.recv().await {
            return Ok(conn);
        }
        return Err(super::TunnelError::CommonError(
            "udp accept error".to_owned(),
        ));
    }

    fn local_url(&self) -> url::Url {
        self.addr.clone()
    }

    fn get_conn_counter(&self) -> Arc<Box<dyn TunnelConnCounter>> {
        struct UdpTunnelConnCounter {
            sock_map: Arc<DashMap<SocketAddr, ArcStreamSinkPair>>,
        }

        impl Debug for UdpTunnelConnCounter {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("UdpTunnelConnCounter")
                    .field("sock_map_len", &self.sock_map.len())
                    .finish()
            }
        }

        impl TunnelConnCounter for UdpTunnelConnCounter {
            fn get(&self) -> u32 {
                self.sock_map.len() as u32
            }
        }

        Arc::new(Box::new(UdpTunnelConnCounter {
            sock_map: self.sock_map.clone(),
        }))
    }
}

pub struct UdpTunnelConnector {
    addr: url::Url,
    bind_addrs: Vec<SocketAddr>,
}

impl UdpTunnelConnector {
    pub fn new(addr: url::Url) -> Self {
        Self {
            addr,
            bind_addrs: vec![],
        }
    }

    async fn wait_sack(
        socket: &UdpSocket,
        addr: SocketAddr,
        conn_id: u32,
    ) -> Result<(), super::TunnelError> {
        let mut buf = BytesMut::new();
        buf.resize(128, 0);

        let (usize, recv_addr) = tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            socket.recv_from(&mut buf),
        )
        .await??;

        if recv_addr != addr {
            return Err(super::TunnelError::ConnectError(format!(
                "udp connect error, unexpected sack addr: {:?}, {:?}",
                recv_addr, addr
            )));
        }

        let _ = buf.split_off(usize);

        let Ok(udp_packet) = rkyv_util::decode_from_bytes_checked::<UdpPacket>(&buf) else {
            tracing::warn!(?buf, "udp decode error in wait sack");
            return Err(super::TunnelError::ConnectError(format!(
                "udp connect error, decode error. buf: {:?}",
                buf
            )));
        };

        if conn_id != udp_packet.conn_id {
            return Err(super::TunnelError::ConnectError(format!(
                "udp connect error, conn id not match. conn_id: {:?}, {:?}",
                conn_id, udp_packet.conn_id
            )));
        }

        if !matches!(udp_packet.payload, ArchivedUdpPacketPayload::Sack) {
            return Err(super::TunnelError::ConnectError(format!(
                "udp connect error, unexpected payload. payload: {:?}",
                udp_packet.payload
            )));
        }

        Ok(())
    }

    async fn wait_sack_loop(
        socket: &UdpSocket,
        addr: SocketAddr,
        conn_id: u32,
    ) -> Result<(), super::TunnelError> {
        while let Err(err) = Self::wait_sack(socket, addr, conn_id).await {
            tracing::warn!(?err, "udp wait sack error");
        }
        Ok(())
    }

    pub async fn try_connect_with_socket(
        &self,
        socket: UdpSocket,
    ) -> Result<Box<dyn super::Tunnel>, super::TunnelError> {
        let addr = super::check_scheme_and_get_socket_addr::<SocketAddr>(&self.addr, "udp")?;
        log::warn!("udp connect: {:?}", self.addr);

        // send syn
        let conn_id = rand::random();
        let udp_packet = UdpPacket::new_syn_packet(conn_id);
        let b = encode_to_bytes::<_, UDP_DATA_MTU>(&udp_packet);
        let ret = socket.send_to(&b, &addr).await?;
        tracing::warn!(?udp_packet, ?ret, "udp send syn");

        // wait sack
        tokio::time::timeout(
            tokio::time::Duration::from_secs(3),
            Self::wait_sack_loop(&socket, addr, conn_id),
        )
        .await??;

        // sack done
        let local_addr = socket.local_addr().unwrap().to_string();
        Ok(Box::new(TunnelWithCustomInfo::new(
            get_tunnel_from_socket(Arc::new(socket), addr, conn_id),
            TunnelInfo {
                tunnel_type: "udp".to_owned(),
                local_addr: super::build_url_from_socket_addr(&local_addr, "udp").into(),
                remote_addr: self.remote_url().into(),
            },
        )))
    }

    async fn connect_with_default_bind(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        return self.try_connect_with_socket(socket).await;
    }

    async fn connect_with_custom_bind(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        let mut futures = FuturesUnordered::new();

        for bind_addr in self.bind_addrs.iter() {
            let socket = UdpSocket::bind(*bind_addr).await?;

            // linux does not use interface of bind_addr to send packet, so we need to bind device
            // mac can handle this with bind correctly
            #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
            if let Some(dev_name) = super::common::get_interface_name_by_ip(&bind_addr.ip()) {
                tracing::trace!(dev_name = ?dev_name, "bind device");
                socket.bind_device(Some(dev_name.as_bytes()))?;
            }

            futures.push(self.try_connect_with_socket(socket));
        }

        let Some(ret) = futures.next().await else {
            return Err(super::TunnelError::CommonError(
                "join connect futures failed".to_owned(),
            ));
        };

        return ret;
    }
}

#[async_trait]
impl super::TunnelConnector for UdpTunnelConnector {
    async fn connect(&mut self) -> Result<Box<dyn super::Tunnel>, super::TunnelError> {
        if self.bind_addrs.is_empty() {
            self.connect_with_default_bind().await
        } else {
            self.connect_with_custom_bind().await
        }
    }

    fn remote_url(&self) -> url::Url {
        self.addr.clone()
    }

    fn set_bind_addrs(&mut self, addrs: Vec<SocketAddr>) {
        self.bind_addrs = addrs;
    }
}

#[cfg(test)]
mod tests {
    use crate::tunnels::common::tests::{_tunnel_bench, _tunnel_pingpong};

    use super::*;

    #[tokio::test]
    async fn udp_pingpong() {
        let listener = UdpTunnelListener::new("udp://0.0.0.0:5556".parse().unwrap());
        let connector = UdpTunnelConnector::new("udp://127.0.0.1:5556".parse().unwrap());
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    async fn udp_bench() {
        let listener = UdpTunnelListener::new("udp://0.0.0.0:5555".parse().unwrap());
        let connector = UdpTunnelConnector::new("udp://127.0.0.1:5555".parse().unwrap());
        _tunnel_bench(listener, connector).await
    }

    #[tokio::test]
    async fn udp_bench_with_bind() {
        let listener = UdpTunnelListener::new("udp://127.0.0.1:5554".parse().unwrap());
        let mut connector = UdpTunnelConnector::new("udp://127.0.0.1:5554".parse().unwrap());
        connector.set_bind_addrs(vec!["127.0.0.1:0".parse().unwrap()]);
        _tunnel_pingpong(listener, connector).await
    }

    #[tokio::test]
    #[should_panic]
    async fn udp_bench_with_bind_fail() {
        let listener = UdpTunnelListener::new("udp://127.0.0.1:5553".parse().unwrap());
        let mut connector = UdpTunnelConnector::new("udp://127.0.0.1:5553".parse().unwrap());
        connector.set_bind_addrs(vec!["10.0.0.1:0".parse().unwrap()]);
        _tunnel_pingpong(listener, connector).await
    }
}