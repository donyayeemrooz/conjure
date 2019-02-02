use libc::size_t;
use std::os::raw::c_void;
use std::panic;
use std::slice;

use pnet::packet::Packet;
use pnet::packet::ethernet::{EthernetPacket, EtherTypes};
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4::Ipv4Packet;
use pnet::packet::ipv6::Ipv6Packet;
use pnet::packet::tcp::{TcpPacket,TcpFlags};
use std::net::{IpAddr,Ipv6Addr,Ipv4Addr};

//use elligator;
use flow_tracker::{Flow, FlowNoSrcPort};
use PerCoreGlobal;
use util::{IpPacket, DDIpSelector};
use elligator;

const TLS_TYPE_APPLICATION_DATA: u8 = 0x17;
//const SQUID_PROXY_ADDR: &'static str = "127.0.0.1";
//const SQUID_PROXY_PORT: u16 = 1234;

//const STREAM_TIMEOUT_NS: u64 = 120*1000*1000*1000; // 120 seconds

fn get_ip_packet<'p>(eth_pkt: &'p EthernetPacket) -> Option<IpPacket<'p>>
{
    let payload = eth_pkt.payload();

    fn parse_v4<'a>(p: &[u8]) -> Option<IpPacket> {
        match Ipv4Packet::new(p) {
            Some(pkt) => Some(IpPacket::V4(pkt)),
            None => None
        }
    }

    fn parse_v6(p: &[u8]) -> Option<IpPacket> {
        match Ipv6Packet::new(p) {
            Some(pkt) => Some(IpPacket::V6(pkt)),
            None => None
        }
    }

    match eth_pkt.get_ethertype() {
        EtherTypes::Vlan => {
            if payload[2] == 0x08 && payload[3] == 0x00 {
                //let vlan_id: u16 = (payload[0] as u16)*256
                //                 + (payload[1] as u16);
                parse_v4(&payload[4..])
            } else if payload[2] == 0x86 && payload[3] == 0xdd {
                parse_v6(&payload[4..])
            } else {
                None
            }
        },
        EtherTypes::Ipv4 => parse_v4(&payload[0..]),
        EtherTypes::Ipv6 => parse_v6(&payload[0..]),
        _ => None,
    }
}


// The jumping off point for all of our logic. This function inspects a packet
// that has come in the tap interface. We do not yet have any idea if we care
// about it; it might not even be TLS. It might not even be TCP!
#[no_mangle]
pub extern "C" fn rust_process_packet(ptr: *mut PerCoreGlobal,
                                      raw_ethframe: *mut c_void,
                                      frame_len: size_t)
{
    #[allow(unused_mut)]
    let mut global = unsafe { &mut *ptr };

    let rust_view_len = frame_len as usize;
    let rust_view = unsafe {
        slice::from_raw_parts_mut(raw_ethframe as *mut u8, frame_len as usize)
    };
    global.stats.packets_this_period += 1;
    global.stats.bytes_this_period += rust_view_len as u64;

    let eth_pkt = match EthernetPacket::new(rust_view) {
        Some(pkt) => pkt,
        None => return,
    };

    match get_ip_packet(&eth_pkt) {
        Some(IpPacket::V4(pkt)) => global.process_ipv4_packet(pkt, rust_view_len),
        Some(IpPacket::V6(pkt)) => global.process_ipv6_packet(pkt, rust_view_len),
        None => return,
    }
}

fn is_tls_app_pkt(tcp_pkt: &TcpPacket) -> bool
{
    let payload = tcp_pkt.payload();
    payload.len() > 5 && payload[0] == TLS_TYPE_APPLICATION_DATA
}

impl PerCoreGlobal
{
    // frame_len is supposed to be the length of the whole Ethernet frame. We're
    // only passing it here for plumbing reasons, and just for stat reporting.
    fn process_ipv4_packet(&mut self, ip_pkt: Ipv4Packet, frame_len: usize)
    {
        self.stats.ipv4_packets_this_period += 1;

        // Ignore packets that aren't TCP
        if ip_pkt.get_next_level_protocol() != IpNextHeaderProtocols::Tcp {
            return;
        }
        let ip = IpPacket::V4(ip_pkt);

        {
            // Check TCP/443
            let tcp_pkt = match ip.tcp() {
                Some(pkt) => pkt,
                None => return,
            };
            self.stats.tcp_packets_this_period += 1;

            // Ignore packets that aren't -> 443.
            // libpnet getters all return host order. Ignore the "u16be" in their
            // docs; interactions with pnet are purely host order.
            if tcp_pkt.get_destination() != 443 {
                return;
            }
        }
        self.stats.tls_packets_this_period += 1; // (HTTPS, really)
        self.stats.tls_bytes_this_period += frame_len as u64;
        self.process_tls_pkt(ip);
    }

    fn process_ipv6_packet(&mut self, ip_pkt: Ipv6Packet, frame_len: usize)
    {
        self.stats.ipv6_packets_this_period += 1;

        if ip_pkt.get_next_header() != IpNextHeaderProtocols::Tcp {
            return;
        }
        let ip = IpPacket::V6(ip_pkt);

        {
            let tcp_pkt = match ip.tcp() {
                Some(pkt) => pkt,
                None => return,
            };
            self.stats.tcp_packets_this_period += 1;

            if tcp_pkt.get_destination() != 443 {
                return;
            }
        }
        self.stats.tls_packets_this_period += 1;
        self.stats.tls_bytes_this_period += frame_len as u64;

        //debug!("v6 -> {} {} bytes", ip_pkt.get_destination(), ip_pkt.get_payload_length());
        self.process_tls_pkt(ip);
    }

    // Takes an IPv4 packet
    // Assumes (for now) that TLS records are in a single TCP packet
    // (no fragmentation).
    // Fragments could be stored in the flow_tracker if needed.
    pub fn process_tls_pkt(&mut self,
                           ip_pkt: IpPacket)
    {
        let tcp_pkt = match ip_pkt.tcp() {
            Some(pkt) => pkt,
            None => return,
        };

        let flow = Flow::new(&ip_pkt, &tcp_pkt);


        // Test if this is to a prefix we care about
        /*
        if let IpPacket::V4(pkt) = &ip_pkt {
            if !self.ip_tree.contains_addr_v4(pkt.get_destination()) {
                self.stats.not_in_tree_this_period += 1;
                return;
            }
        }
        self.stats.in_tree_this_period += 1;
        */

        if panic::catch_unwind(||{ tcp_pkt.payload(); }).is_err() {
            return;
        }

        let dd_flow = FlowNoSrcPort::from_flow(&flow);
        if flow.src_ip == IpAddr::from([128u8, 138u8, 244u8, 42u8]) {
            debug!("dd_flow {} {:?}", dd_flow, self.flow_tracker.dark_decoy_flows);
        }
        if self.flow_tracker.is_registered_dark_decoy(&dd_flow) {
            // Tagged flow! Forward packet to whatever
            debug!("Tagged flow packet {}", flow);

            // Update expire time
            self.flow_tracker.mark_dark_decoy(&dd_flow);

            // Forward packet...
            self.forward_pkt(&ip_pkt);
            // TODO: if it was RST or FIN, close things
            return;
        }

        let tcp_flags = tcp_pkt.get_flags();
        if (tcp_flags & TcpFlags::SYN) != 0 && (tcp_flags & TcpFlags::ACK) == 0
        {
            self.stats.port_443_syns_this_period += 1;

            self.flow_tracker.begin_tracking_flow(&flow);
            return;
        } else if (tcp_flags & TcpFlags::RST) != 0 || (tcp_flags & TcpFlags::FIN) != 0 {
            self.flow_tracker.stop_tracking_flow(&flow);
            return;
        }

        if !self.flow_tracker.is_tracked_flow(&flow) {
            return;
        }

        if  is_tls_app_pkt(&tcp_pkt) {
            match self.check_dark_decoy_tag(&flow, &tcp_pkt) {
                Some(dd_flow) => {
                    debug!("New Dark Decoy Flow {} negotiated in {},", dd_flow, flow);
                    self.flow_tracker.mark_dark_decoy(&dd_flow);
                    // not removing flow from stale_tracked_flows for optimization reasons:
                    // it will be removed later
                },
                None => {}
            };
            self.flow_tracker.stop_tracking_flow(&flow);
        }
    }

    fn forward_pkt(&mut self, ip_pkt: &IpPacket)
    {
        let data = match ip_pkt {
            IpPacket::V4(p) => p.packet(),
            IpPacket::V6(p) => p.packet(),
        };

        let mut tun_pkt = Vec::with_capacity(data.len()+4);
        // These mystery bytes are a link-layer header; the kernel "receives"
        // tun packets as if they were really physically "received". Since they
        // weren't physically received, they do not have an Ethernet header. It
        // looks like the tun setup has its own type of header, rather than just
        // making up a fake Ethernet header.
        tun_pkt.extend_from_slice(&[0x00, 0x01, 0x08, 0x00]);
        tun_pkt.extend_from_slice(data);

        self.tun.send(tun_pkt).unwrap_or_else(|e|{
            warn!("failed to send packet into tun: {}", e); 0});

    }

    fn check_dark_decoy_tag(&mut self,
                            flow: &Flow,
                            tcp_pkt: &TcpPacket) -> Option<FlowNoSrcPort>
    {
        self.stats.elligator_this_period += 1;
        match elligator::extract_payloads(&self.priv_key, &tcp_pkt.payload()) {
            Ok(res) => {
                let dd_ip_selector = match DDIpSelector::new(&vec![String::from("192.122.190.0/24")]) {
                                                                       //String::from("2001:48a8:687f:1::/64")]) {
                    // TODO: move this initialization up
                    Ok(dd) => dd,
                    Err(e) => {
                        error!("failed to make Dark Decoy IP selector: {}", e);
                        return None;
                    }
                };

                let dst_ip = match dd_ip_selector.select(res.0.dark_decoy_seed){
                    Some(ip) => ip,
                    None => {
                        error!("failed to select dark decoy IP address");
                        return None;
                    }
                };

                // Send dark_decoy_seed / dst_ip to ZMQ
                //pub dark_decoy_seed: [u8; 16],
                let mut msg = vec![0; 16+16];
                msg[..16].clone_from_slice(&res.0.dark_decoy_seed);

                let ip_as_bytes = match dst_ip {
                    IpAddr::V6(ip) => ip.octets().to_vec(),
                    IpAddr::V4(ip) => {
                        // Convert to Ipv6-mapped v4 address
                        let mut v6 = vec![0; 16];
                        v6[10] = 0xff;
                        v6[11] = 0xff;
                        v6[12..].clone_from_slice(&ip.octets());
                        v6
                    },
                };
                msg[16..].clone_from_slice(&ip_as_bytes[..]);
                self.zmq_sock.send(&msg, 0);

                return Some(FlowNoSrcPort::from_parts(flow.src_ip, dst_ip, 443));
                //return Some(FlowNoSrcPort::from_parts(flow.src_ip, IpAddr::from([192u8, 122u8, 190u8, 106u8]), 443));
            },
            Err(_e) => {
                return None;
            }
        }
    }
} // impl PerCoreGlobal
