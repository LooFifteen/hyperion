//! All the networking related code.

use std::{
    cell::RefCell,
    collections::VecDeque,
    hash::Hash,
    net::ToSocketAddrs,
    sync::{atomic, atomic::AtomicUsize},
};

use derive_more::{Deref, DerefMut, From};
use evenio::{fetch::Single, handler::HandlerParam, prelude::Component};
use libc::iovec;
use libdeflater::CompressionLvl;
use tracing::{debug, trace};
use valence_protocol::CompressionThreshold;

use crate::{global::Global, net::encoder::PacketWriteInfo, singleton::ring::Ring};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(not(target_os = "linux"))]
mod generic;

#[derive(Debug, Copy, Clone, Component, PartialEq, Eq, Hash)]
pub struct Fd(
    #[cfg(target_os = "linux")] linux::Fixed,
    #[cfg(not(target_os = "linux"))] usize,
);

#[allow(unused, reason = "these are used on linux")]
pub enum ServerEvent<'a> {
    AddPlayer { fd: Fd },
    RemovePlayer { fd: Fd },
    RecvData { fd: Fd, data: &'a [u8] },
    SentData { fd: Fd },
}

pub struct Server {
    #[cfg(target_os = "linux")]
    server: linux::LinuxServer,
    #[cfg(not(target_os = "linux"))]
    server: generic::GenericServer,
}

impl ServerDef for Server {
    #[allow(unused, reason = "this has to do with cross-platform code")]
    fn new(address: impl ToSocketAddrs) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                server: linux::LinuxServer::new(address)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self {
                server: generic::GenericServer::new(address)?,
            })
        }
    }

    fn drain(&mut self, f: impl FnMut(ServerEvent)) -> std::io::Result<()> {
        self.server.drain(f)
    }

    fn allocate_buffers(&mut self, buffers: &[iovec]) {
        for (idx, elem) in buffers.iter().enumerate() {
            let ptr = elem.iov_base as *const u8;
            let len = elem.iov_len;
            let len_readable = humansize::SizeFormatter::new(len, humansize::BINARY);
            debug!("buffer {idx} {ptr:?} of len {len} = {len_readable}");
        }

        self.server.allocate_buffers(buffers);
    }

    /// Impl with local sends BEFORE broadcasting
    fn write_all<'a>(
        &mut self,
        global: &mut Global,
        writers: impl Iterator<Item = RefreshItems<'a>>,
    ) {
        self.server.write_all(global, writers);
    }

    fn submit_events(&mut self) {
        self.server.submit_events();
    }
}

#[allow(unused, reason = "this is used on linux")]
pub struct RefreshItems<'a> {
    pub write: &'a mut RayonLocal<VecDeque<PacketWriteInfo>>,
    pub fd: Fd,
}

pub trait ServerDef {
    fn new(address: impl ToSocketAddrs) -> anyhow::Result<Self>
    where
        Self: Sized;
    fn drain(&mut self, f: impl FnMut(ServerEvent)) -> std::io::Result<()>;

    // todo:make unsafe
    fn allocate_buffers(&mut self, buffers: &[iovec]);

    fn write_all<'a>(
        &mut self,
        global: &mut Global,
        writers: impl Iterator<Item = RefreshItems<'a>>,
    );

    fn submit_events(&mut self);
}

struct NotImplemented;

impl ServerDef for NotImplemented {
    fn new(_address: impl ToSocketAddrs) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        unimplemented!("not implemented; use Linux")
    }

    fn drain(&mut self, _f: impl FnMut(ServerEvent)) -> std::io::Result<()> {
        unimplemented!("not implemented; use Linux")
    }

    fn allocate_buffers(&mut self, _buffers: &[iovec]) {
        unimplemented!("not implemented; use Linux")
    }

    fn write_all<'a>(
        &mut self,
        _global: &mut Global,
        _writers: impl Iterator<Item = RefreshItems<'a>>,
    ) {
        unimplemented!("not implemented; use Linux")
    }

    fn submit_events(&mut self) {
        unimplemented!("not implemented; use Linux")
    }
}

/// The Minecraft protocol version this library currently targets.
pub const PROTOCOL_VERSION: i32 = 763;

// todo: this is one off.. why?
// pub const MAX_PACKET_SIZE: usize = 0x001F_FFFF;
/// The maximum number of bytes that can be sent in a single packet.
pub const MAX_PACKET_SIZE: usize = valence_protocol::MAX_PACKET_SIZE as usize;

/// The stringified name of the Minecraft version this library currently
/// targets.
pub const MINECRAFT_VERSION: &str = "1.20.1";

mod decoder;
pub mod encoder;

pub use decoder::PacketDecoder;
use rayon_local::RayonLocal;

use crate::{
    event::Scratches, net::encoder::append_packet_without_compression,
    singleton::ring::register_rings,
};

// 128 MiB * num_cores
pub const S2C_BUFFER_SIZE: usize = 1024 * 1024 * 128;

#[derive(Debug)]
pub struct IoBuf {
    /// The encoding buffer and logic
    enc: encoder::PacketEncoder,
    buf: Ring,
    index: usize,
}

#[derive(Component, Deref, DerefMut)]
pub struct Compressors {
    compressors: RayonLocal<RefCell<libdeflater::Compressor>>,
}

impl Compressors {
    #[must_use]
    pub fn new(level: CompressionLvl) -> Self {
        Self {
            compressors: RayonLocal::init(|| libdeflater::Compressor::new(level).into()),
        }
    }
}

#[derive(Component, Debug, Deref, DerefMut)]
pub struct IoBufs {
    locals: RayonLocal<RefCell<IoBuf>>,
}

impl IoBufs {
    pub fn init(threshold: CompressionThreshold, server_def: &mut impl ServerDef) -> Self {
        let mut locals = RayonLocal::init_with_index(|i| IoBuf::new(threshold, i));

        let rings = locals.get_all_mut().iter_mut().map(IoBuf::buf_mut);
        register_rings(server_def, rings);

        let locals = locals.map(RefCell::new);

        Self { locals }
    }
}

impl IoBuf {
    #[must_use]
    pub fn new(threshold: CompressionThreshold, index: usize) -> Self {
        Self {
            enc: encoder::PacketEncoder::new(threshold),
            buf: Ring::new(S2C_BUFFER_SIZE),
            index,
        }
    }

    #[must_use]
    pub const fn enc(&self) -> &encoder::PacketEncoder {
        &self.enc
    }

    pub fn enc_mut(&mut self) -> &mut encoder::PacketEncoder {
        &mut self.enc
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    pub fn buf_mut(&mut self) -> &mut Ring {
        &mut self.buf
    }
}

#[derive(HandlerParam, Copy, Clone)]
pub struct Compose<'a> {
    pub bufs: Single<'a, &'static IoBufs>,
    pub compressor: Single<'a, &'static Compressors>,
    pub scratch: Single<'a, &'static Scratches>,
}

/// This is useful for the ECS so we can use Single<&mut Broadcast> instead of having to use a marker struct
#[derive(Component, From, Deref, DerefMut, Default)]
pub struct Broadcast(Packets);

/// Stores indices of packets
#[derive(Component, Default)]
pub struct Packets {
    to_write: RayonLocal<VecDeque<PacketWriteInfo>>,
    number_sending: AtomicUsize,
}

impl Packets {
    pub fn extend(&mut self, other: &Self) {
        let this = self.to_write.iter_mut();
        let other = other.to_write.iter();

        for (this, other) in this.zip(other) {
            this.extend(other);
        }
    }

    pub fn get_write_mut(&mut self) -> &mut RayonLocal<VecDeque<PacketWriteInfo>> {
        &mut self.to_write
    }

    #[must_use]
    pub fn can_send(&self) -> bool {
        if self.number_sending.load(atomic::Ordering::Relaxed) != 0 {
            return false;
        }

        self.to_write.iter().any(|x| !x.is_empty())
    }

    pub fn set_successfully_sent(&self, d_count: usize) {
        debug_assert!(
            self.number_sending.load(atomic::Ordering::Relaxed) > 0,
            "somehow number sending is 0 even though we just marked a successful send"
        );

        self.number_sending
            .fetch_sub(d_count, atomic::Ordering::Relaxed);
    }

    pub fn prepare_for_send(&mut self) -> usize {
        debug_assert!(
            self.number_sending.load(atomic::Ordering::Relaxed) == 0,
            "number sending is not 0 even though we are preparing for send"
        );
        let count = self.to_write.iter().map(VecDeque::len).sum();
        self.number_sending = AtomicUsize::new(count);
        count
    }

    pub fn clear(&mut self) {
        self.to_write.iter_mut().for_each(VecDeque::clear);
    }

    fn push(&self, writer: PacketWriteInfo, buf: &IoBuf) {
        let idx = buf.index();
        let to_write = unsafe { &mut *self.to_write.get_raw(idx).get() };

        if let Some(last) = to_write.back_mut() {
            let start_pointer_if_contiguous = unsafe { last.start_ptr.add(last.len as usize) };
            if start_pointer_if_contiguous == writer.start_ptr {
                last.len += writer.len;
                return;
            }
        }

        to_write.push_back(writer);
    }

    pub fn append_pre_compression_packet<P>(&self, pkt: &P, buf: &mut IoBuf) -> anyhow::Result<()>
    where
        P: valence_protocol::Packet + valence_protocol::Encode,
    {
        let compression = buf.enc.compression_threshold();
        // none
        buf.enc.set_compression(CompressionThreshold::DEFAULT);

        let result = append_packet_without_compression(pkt, &mut buf.buf)?;

        trace!("without compression: {result:?}");

        self.push(result, buf);

        // reset
        buf.enc.set_compression(compression);

        Ok(())
    }

    pub fn append<P>(&self, pkt: &P, compose: &Compose) -> anyhow::Result<()>
    where
        P: valence_protocol::Packet + valence_protocol::Encode,
    {
        let buf = compose.bufs.get_local();

        let scratch = compose.scratch.get_local();
        let mut scratch = scratch.borrow_mut();

        let compressor = compose.compressor.get_local();
        let mut compressor = compressor.borrow_mut();

        let mut buf = buf.borrow_mut();
        let buf = &mut *buf;

        let result = buf
            .enc
            .append_packet(pkt, &mut buf.buf, &mut *scratch, &mut compressor)?;

        self.push(result, buf);
        Ok(())
    }

    pub fn append_raw(&self, data: &[u8], buf: &mut IoBuf) {
        let start_ptr = buf.buf.append(data);

        let writer = PacketWriteInfo {
            start_ptr,
            len: data.len() as u32,
        };

        self.push(writer, buf);
    }
}

// #[cfg(test)]
// mod tests {
//     use bumpalo::Bump;
//     use valence_protocol::{packets::login::LoginHelloC2s, Bounded};
//
//     use super::*;
//     use crate::events::Scratch;
//
//     #[test]
//     fn test_append_pre_compression_packet() {
//         let mut buf = IoBuf::new(
//             CompressionThreshold::DEFAULT,
//             CompressionLvl::new(4).unwrap(),
//         );
//         let mut packets = Packets::default();
//
//         let pkt = LoginHelloC2s {
//             username: Bounded::default(),
//             profile_id: None,
//         };
//
//         let bump = Bump::new();
//         let mut scratch = Scratch::from(&bump);
//
//         packets
//             .append_pre_compression_packet(&pkt, &mut buf)
//             .unwrap();
//
//         assert_eq!(packets.get_write().len(), 1);
//
//         let len = packets.get_write()[0].len;
//
//         assert_eq!(len, 4); // Packet length for an empty LoginHelloC2s
//     }
//     #[test]
//     fn test_append_packet() {
//         let mut buf = IoBuf::new(
//             CompressionThreshold::DEFAULT,
//             CompressionLvl::new(4).unwrap(),
//         );
//         let mut packets = Packets::default();
//
//         let pkt = LoginHelloC2s {
//             username: Bounded::default(),
//             profile_id: None,
//         };
//
//         let bump = Bump::new();
//         let mut scratch = Scratch::from(&bump);
//         packets.append(&pkt, &mut buf, &mut scratch).unwrap();
//
//         assert_eq!(packets.get_write().len(), 1);
//         let len = packets.get_write()[0].len;
//         assert_eq!(len, 4); // Packet length for an empty LoginHelloC2s
//     }
//
//     #[test]
//     fn test_append_raw() {
//         let mut buf = IoBuf::new(
//             CompressionThreshold::DEFAULT,
//             CompressionLvl::new(4).unwrap(),
//         );
//         let mut packets = Packets::default();
//
//         let data = b"Hello, world!";
//         packets.append_raw(data, &mut buf);
//
//         assert_eq!(packets.get_write().len(), 1);
//
//         let len = packets.get_write()[0].len;
//         assert_eq!(len, data.len() as u32);
//     }
//
//     #[test]
//     fn test_clear() {
//         let mut buf = IoBuf::new(
//             CompressionThreshold::DEFAULT,
//             CompressionLvl::new(4).unwrap(),
//         );
//         let mut packets = Packets::default();
//
//         let pkt = LoginHelloC2s {
//             username: Bounded::default(),
//             profile_id: None,
//         };
//
//         let bump = Bump::new();
//         let mut scratch = Scratch::from(&bump);
//
//         packets.append(&pkt, &mut buf, &mut scratch).unwrap();
//         assert_eq!(packets.get_write().len(), 1);
//
//         packets.clear();
//         assert_eq!(packets.get_write().len(), 0);
//     }
//
//     #[test]
//     fn test_contiguous_packets() {
//         let mut buf = IoBuf::new(
//             CompressionThreshold::DEFAULT,
//             CompressionLvl::new(4).unwrap(),
//         );
//         let mut packets = Packets::default();
//
//         let pkt1 = LoginHelloC2s {
//             username: Bounded::default(),
//             profile_id: None,
//         };
//         let pkt2 = LoginHelloC2s {
//             username: Bounded::default(),
//             profile_id: None,
//         };
//
//         let bump = Bump::new();
//         let mut scratch = Scratch::from(&bump);
//
//         packets
//             .append_pre_compression_packet(&pkt1, &mut buf, &mut scratch)
//             .unwrap();
//         packets
//             .append_pre_compression_packet(&pkt2, &mut buf, &mut scratch)
//             .unwrap();
//
//         assert_eq!(packets.get_write().len(), 1);
//
//         let len = packets.get_write()[0].len;
//         assert_eq!(len, 8); // Combined length of both packets
//     }
// }
