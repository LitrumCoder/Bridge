//! The default gateway and its MAC from the IP Helper tables (no location permission,
//! unlike the Wi-Fi SSID on Windows 11 24H2+).

use std::net::Ipv4Addr;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetBestRoute, GetIpAddrTable, GetIpNetTable, MIB_IPADDRTABLE, MIB_IPFORWARDROW, MIB_IPNETTABLE,
};

use crate::network::NetworkInfo;

/// Calls a "size, then fill" IP Helper function into a properly aligned buffer.
fn table<T>(call: impl Fn(*mut T, *mut u32) -> u32) -> Option<Vec<u64>> {
    let mut size = 0u32;
    call(std::ptr::null_mut(), &mut size);
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u64; (size as usize).div_ceil(8)];
    (call(buf.as_mut_ptr().cast(), &mut size) == 0).then_some(buf)
}

pub(crate) fn current_network() -> Option<NetworkInfo> {
    let mut route: MIB_IPFORWARDROW = unsafe_zeroed();
    // SAFETY: out-pointer to a local; 8.8.8.8 just selects the default route, nothing
    // is sent.
    if unsafe { GetBestRoute(u32::from_ne_bytes([8, 8, 8, 8]), 0, &mut route) } != 0 {
        return None;
    }
    let gateway = Ipv4Addr::from(route.dwForwardNextHop.to_ne_bytes());
    if gateway.is_unspecified() {
        return None;
    }
    let iface = route.dwForwardIfIndex;

    // SAFETY: the buffer is sized by the first call and holds a MIB_IPNETTABLE.
    let net = table::<MIB_IPNETTABLE>(|p, s| unsafe { GetIpNetTable(p, s, 0) })?;
    // SAFETY: as above; rows are read within dwNumEntries.
    let gateway_mac = unsafe {
        let t = &*(net.as_ptr() as *const MIB_IPNETTABLE);
        let rows = std::slice::from_raw_parts(t.table.as_ptr(), t.dwNumEntries as usize);
        rows.iter()
            .find(|r| {
                r.dwIndex == iface && r.dwAddr == route.dwForwardNextHop && r.dwPhysAddrLen == 6
            })
            .map(|r| {
                let mut m = [0u8; 6];
                m.copy_from_slice(&r.bPhysAddr[..6]);
                m
            })
    };

    // SAFETY: as above, a MIB_IPADDRTABLE.
    let addrs = table::<MIB_IPADDRTABLE>(|p, s| unsafe { GetIpAddrTable(p, s, 0) })?;
    // SAFETY: rows within dwNumEntries.
    let (subnet, prefix_len) = unsafe {
        let t = &*(addrs.as_ptr() as *const MIB_IPADDRTABLE);
        let rows = std::slice::from_raw_parts(t.table.as_ptr(), t.dwNumEntries as usize);
        let r = rows.iter().find(|r| r.dwIndex == iface)?;
        let net = Ipv4Addr::from((r.dwAddr & r.dwMask).to_ne_bytes());
        (
            net,
            u32::from(Ipv4Addr::from(r.dwMask.to_ne_bytes())).leading_ones() as u8,
        )
    };
    Some(NetworkInfo {
        gateway,
        gateway_mac,
        subnet,
        prefix_len,
    })
}

fn unsafe_zeroed() -> MIB_IPFORWARDROW {
    // SAFETY: a plain C struct of integers and unions of integers.
    unsafe { std::mem::zeroed() }
}
