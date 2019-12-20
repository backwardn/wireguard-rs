use std::sync::atomic::Ordering;
use std::time::Instant;

use byteorder::{ByteOrder, LittleEndian};
use crossbeam_channel::Receiver;
use log::debug;
use rand::rngs::OsRng;
use x25519_dalek::PublicKey;

// IO traits
use super::Endpoint;

use super::tun::Reader as TunReader;
use super::tun::Tun;

use super::udp::Reader as UDPReader;
use super::udp::Writer as UDPWriter;
use super::udp::UDP;

// constants
use super::constants::{
    DURATION_UNDER_LOAD, MESSAGE_PADDING_MULTIPLE, THRESHOLD_UNDER_LOAD, TIME_HORIZON,
};
use super::handshake::MAX_HANDSHAKE_MSG_SIZE;
use super::handshake::{TYPE_COOKIE_REPLY, TYPE_INITIATION, TYPE_RESPONSE};
use super::router::{CAPACITY_MESSAGE_POSTFIX, SIZE_MESSAGE_PREFIX, TYPE_TRANSPORT};

use super::Wireguard;

pub enum HandshakeJob<E> {
    Message(Vec<u8>, E),
    New(PublicKey),
}

/* Returns the padded length of a message:
 *
 * # Arguments
 *
 * - `size` : Size of unpadded message
 * - `mtu` : Maximum transmission unit of the device
 *
 * # Returns
 *
 * The padded length (always less than or equal to the MTU)
 */
#[inline(always)]
const fn padding(size: usize, mtu: usize) -> usize {
    #[inline(always)]
    const fn min(a: usize, b: usize) -> usize {
        let m = (a < b) as usize;
        a * m + (1 - m) * b
    }
    let pad = MESSAGE_PADDING_MULTIPLE;
    min(mtu, size + (pad - size % pad) % pad)
}

pub fn tun_worker<T: Tun, B: UDP>(wg: &Wireguard<T, B>, reader: T::Reader) {
    loop {
        // create vector big enough for any transport message (based on MTU)
        let mtu = wg.mtu.load(Ordering::Relaxed);
        let size = mtu + SIZE_MESSAGE_PREFIX + 1;
        let mut msg: Vec<u8> = vec![0; size + CAPACITY_MESSAGE_POSTFIX];

        // read a new IP packet
        let payload = match reader.read(&mut msg[..], SIZE_MESSAGE_PREFIX) {
            Ok(payload) => payload,
            Err(e) => {
                debug!("TUN worker, failed to read from tun device: {}", e);
                break;
            }
        };
        debug!("TUN worker, IP packet of {} bytes (MTU = {})", payload, mtu);

        // check if device is down
        if mtu == 0 {
            continue;
        }

        // truncate padding
        let padded = padding(payload, mtu);
        log::trace!(
            "TUN worker, payload length = {}, padded length = {}",
            payload,
            padded
        );
        msg.truncate(SIZE_MESSAGE_PREFIX + padded);
        debug_assert!(padded <= mtu);
        debug_assert_eq!(
            if padded < mtu {
                (msg.len() - SIZE_MESSAGE_PREFIX) % MESSAGE_PADDING_MULTIPLE
            } else {
                0
            },
            0
        );

        // crypt-key route
        let e = wg.router.send(msg);
        debug!("TUN worker, router returned {:?}", e);
    }
}

pub fn udp_worker<T: Tun, B: UDP>(wg: &Wireguard<T, B>, reader: B::Reader) {
    let mut last_under_load = Instant::now() - TIME_HORIZON;

    loop {
        // create vector big enough for any message given current MTU
        let mtu = wg.mtu.load(Ordering::Relaxed);
        let size = mtu + MAX_HANDSHAKE_MSG_SIZE;
        let mut msg: Vec<u8> = vec![0; size];

        // read UDP packet into vector
        let (size, src) = match reader.read(&mut msg) {
            Err(e) => {
                debug!("Bind reader closed with {}", e);
                return;
            }
            Ok(v) => v,
        };
        msg.truncate(size);

        // TODO: start device down
        if mtu == 0 {
            continue;
        }

        // message type de-multiplexer
        if msg.len() < std::mem::size_of::<u32>() {
            continue;
        }
        match LittleEndian::read_u32(&msg[..]) {
            TYPE_COOKIE_REPLY | TYPE_INITIATION | TYPE_RESPONSE => {
                debug!("{} : reader, received handshake message", wg);

                // add one to pending
                let pending = wg.pending.fetch_add(1, Ordering::SeqCst);

                // update under_load flag
                if pending > THRESHOLD_UNDER_LOAD {
                    debug!("{} : reader, set under load (pending = {})", wg, pending);
                    last_under_load = Instant::now();
                } else if last_under_load.elapsed() > DURATION_UNDER_LOAD {
                    debug!("{} : reader, clear under load", wg);
                }

                // add to handshake queue
                wg.queue.send(HandshakeJob::Message(msg, src));
            }
            TYPE_TRANSPORT => {
                debug!("{} : reader, received transport message", wg);

                // transport message
                let _ = wg.router.recv(src, msg).map_err(|e| {
                    debug!("Failed to handle incoming transport message: {}", e);
                });
            }
            _ => (),
        }
    }
}

pub fn handshake_worker<T: Tun, B: UDP>(
    wg: &Wireguard<T, B>,
    rx: Receiver<HandshakeJob<B::Endpoint>>,
) {
    debug!("{} : handshake worker, started", wg);

    // prepare OsRng instance for this thread
    let mut rng = OsRng::new().expect("Unable to obtain a CSPRNG");

    // process elements from the handshake queue
    for job in rx {
        // decrement pending pakcets (under_load)
        let job: HandshakeJob<B::Endpoint> = job;
        wg.pending.fetch_sub(1, Ordering::SeqCst);

        // demultiplex staged handshake jobs and handshake messages
        match job {
            HandshakeJob::Message(msg, src) => {
                // feed message to handshake device
                let src_validate = (&src).into_address(); // TODO avoid

                // process message
                let device = wg.handshake.read();
                match device.process(
                    &mut rng,
                    &msg[..],
                    None,
                    /*
                    if wg.under_load.load(Ordering::Relaxed) {
                        debug!("{} : handshake worker, under load", wg);
                        Some(&src_validate)
                    } else {
                        None
                    }
                    */
                ) {
                    Ok((pk, resp, keypair)) => {
                        // send response (might be cookie reply or handshake response)
                        let mut resp_len: u64 = 0;
                        if let Some(msg) = resp {
                            resp_len = msg.len() as u64;
                            let send: &Option<B::Writer> = &*wg.send.read();
                            if let Some(writer) = send.as_ref() {
                                debug!(
                                    "{} : handshake worker, send response ({} bytes)",
                                    wg, resp_len
                                );
                                let _ = writer.write(&msg[..], &src).map_err(|e| {
                                    debug!(
                                        "{} : handshake worker, failed to send response, error = {}",
                                        wg,
                                        e
                                    )
                                });
                            }
                        }

                        // update peer state
                        if let Some(pk) = pk {
                            // authenticated handshake packet received
                            if let Some(peer) = wg.peers.read().get(pk.as_bytes()) {
                                // add to rx_bytes and tx_bytes
                                let req_len = msg.len() as u64;
                                peer.rx_bytes.fetch_add(req_len, Ordering::Relaxed);
                                peer.tx_bytes.fetch_add(resp_len, Ordering::Relaxed);

                                // update endpoint
                                peer.router.set_endpoint(src);

                                if resp_len > 0 {
                                    // update timers after sending handshake response
                                    debug!("{} : handshake worker, handshake response sent", wg);
                                    peer.state.sent_handshake_response();
                                } else {
                                    // update timers after receiving handshake response
                                    debug!(
                                        "{} : handshake worker, handshake response was received",
                                        wg
                                    );
                                    peer.state.timers_handshake_complete();
                                }

                                // add any new keypair to peer
                                keypair.map(|kp| {
                                    debug!("{} : handshake worker, new keypair for {}", wg, peer);

                                    // this means that a handshake response was processed or sent
                                    peer.timers_session_derived();

                                    // free any unused ids
                                    for id in peer.router.add_keypair(kp) {
                                        device.release(id);
                                    }
                                });
                            }
                        }
                    }
                    Err(e) => debug!("{} : handshake worker, error = {:?}", wg, e),
                }
            }
            HandshakeJob::New(pk) => {
                if let Some(peer) = wg.peers.read().get(pk.as_bytes()) {
                    debug!(
                        "{} : handshake worker, new handshake requested for {}",
                        wg, peer
                    );
                    let device = wg.handshake.read();
                    let _ = device.begin(&mut rng, &peer.pk).map(|msg| {
                        let _ = peer.router.send(&msg[..]).map_err(|e| {
                            debug!("{} : handshake worker, failed to send handshake initiation, error = {}", wg, e)
                        });
                        peer.state.sent_handshake_initiation();
                    });
                    peer.handshake_queued.store(false, Ordering::SeqCst);
                }
            }
        }
    }
}
