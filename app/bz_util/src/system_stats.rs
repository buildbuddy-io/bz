/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use crate::threads::available_parallelism;

use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use crate::os::host_cpu_usage::HostCpuUsage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemMemoryStats {
    pub total: u64,
    pub available: u64,
}

pub struct UnixSystemStats {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
}

impl UnixSystemStats {
    #[cfg(unix)]
    pub fn get() -> Option<Self> {
        let mut loadavg: [f64; 3] = [0.0, 0.0, 0.0];
        if unsafe { libc::getloadavg(&mut loadavg[0], 3) } != 3 {
            // This doesn't seem to set errno (or at least it's not documented to do so).
            return None;
        }
        Some(Self {
            load1: loadavg[0],
            load5: loadavg[1],
            load15: loadavg[2],
        })
    }

    #[cfg(not(unix))]
    pub fn get() -> Option<Self> {
        None
    }
}

/// Returns the number of CPU cores on the system.
#[cfg(unix)]
pub fn num_cores() -> usize {
    use std::sync::OnceLock;
    static NUM_CORES: OnceLock<usize> = OnceLock::new();
    *NUM_CORES.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if n < 1 {
            available_parallelism()
        } else {
            n as usize
        }
    })
}

#[cfg(not(unix))]
pub fn num_cores() -> usize {
    available_parallelism()
}

pub fn system_memory_stats() -> u64 {
    system_memory_stats_detailed().total
}

pub fn system_memory_stats_detailed() -> SystemMemoryStats {
    if let Ok(Some(bytes)) = bz_env::env::bz_env!("BUCK2_TEST_FAKE_SYSTEM_TOTAL_MEMORY", type=u64, applicability=testing)
    {
        let available = bz_env::env::bz_env!("BUCK2_TEST_FAKE_SYSTEM_AVAILABLE_MEMORY", type=u64, applicability=testing)
            .ok()
            .flatten()
            .unwrap_or(bytes);
        return SystemMemoryStats {
            total: bytes,
            available,
        };
    }

    use sysinfo::MemoryRefreshKind;
    use sysinfo::RefreshKind;
    use sysinfo::System;

    let system = System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    );
    SystemMemoryStats {
        total: system.total_memory(),
        available: system.available_memory(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemDiskIoStats {
    pub read_bytes_per_second: u64,
    pub write_bytes_per_second: u64,
    pub read_operations_per_second: Option<u64>,
    pub write_operations_per_second: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SystemDiskIoOperations {
    read_operations: u64,
    write_operations: u64,
}

pub struct SystemDiskIoStatsCollector {
    disks: sysinfo::Disks,
    last_refresh: Option<Instant>,
    last_operations: Option<SystemDiskIoOperations>,
}

impl Default for SystemDiskIoStatsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemDiskIoStatsCollector {
    pub fn new() -> Self {
        Self {
            disks: sysinfo::Disks::new_with_refreshed_list_specifics(
                disk_io_refresh_kind(),
            ),
            last_refresh: None,
            last_operations: None,
        }
    }

    pub fn refresh(&mut self) -> Option<SystemDiskIoStats> {
        let now = Instant::now();
        let previous_refresh = self.last_refresh.replace(now);
        let previous_operations = self.last_operations;
        for disk in self.disks.list_mut() {
            disk.refresh_specifics(disk_io_refresh_kind());
        }
        let current_operations = collect_disk_io_operations(&self.disks);
        self.last_operations = current_operations;

        let previous_refresh = match previous_refresh {
            Some(previous_refresh) => previous_refresh,
            None => {
                return Some(SystemDiskIoStats {
                    read_bytes_per_second: 0,
                    write_bytes_per_second: 0,
                    read_operations_per_second: current_operations.map(|_| 0),
                    write_operations_per_second: current_operations.map(|_| 0),
                });
            }
        };

        let elapsed = now.checked_duration_since(previous_refresh)?;
        if elapsed.is_zero() {
            return None;
        }

        let mut read_bytes = 0_u64;
        let mut write_bytes = 0_u64;
        for disk in self.disks.list() {
            let usage = disk.usage();
            read_bytes = read_bytes.saturating_add(usage.read_bytes);
            write_bytes = write_bytes.saturating_add(usage.written_bytes);
        }

        Some(SystemDiskIoStats {
            read_bytes_per_second: bytes_per_second(read_bytes, elapsed),
            write_bytes_per_second: bytes_per_second(write_bytes, elapsed),
            read_operations_per_second: operations_per_second(
                previous_operations,
                current_operations,
                elapsed,
                |operations| operations.read_operations,
            ),
            write_operations_per_second: operations_per_second(
                previous_operations,
                current_operations,
                elapsed,
                |operations| operations.write_operations,
            ),
        })
    }
}

fn disk_io_refresh_kind() -> sysinfo::DiskRefreshKind {
    sysinfo::DiskRefreshKind::nothing().with_io_usage()
}

fn bytes_per_second(bytes: u64, elapsed: Duration) -> u64 {
    rate_per_second(bytes, elapsed)
}

fn operations_per_second(
    previous: Option<SystemDiskIoOperations>,
    current: Option<SystemDiskIoOperations>,
    elapsed: Duration,
    select: impl Fn(SystemDiskIoOperations) -> u64,
) -> Option<u64> {
    let previous = previous?;
    let current = current?;
    Some(rate_per_second(
        select(current).saturating_sub(select(previous)),
        elapsed,
    ))
}

fn rate_per_second(count: u64, elapsed: Duration) -> u64 {
    let elapsed_secs = elapsed.as_secs_f64();
    if elapsed_secs <= 0.0 {
        return 0;
    }
    (count as f64 / elapsed_secs).round() as u64
}

fn collect_disk_io_operations(disks: &sysinfo::Disks) -> Option<SystemDiskIoOperations> {
    #[cfg(target_os = "linux")]
    {
        collect_linux_disk_io_operations(disks)
    }

    #[cfg(target_os = "macos")]
    {
        collect_macos_disk_io_operations(disks)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _disks = disks;
        None
    }
}

#[cfg(target_os = "linux")]
fn collect_linux_disk_io_operations(disks: &sysinfo::Disks) -> Option<SystemDiskIoOperations> {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::path::PathBuf;
    use std::str::FromStr;

    #[derive(Clone, Copy)]
    struct LinuxDiskIoOperations {
        read_operations: u64,
        write_operations: u64,
    }

    fn actual_device_name(device: &OsStr) -> String {
        let device_path = PathBuf::from(device);

        std::fs::canonicalize(&device_path)
            .ok()
            .and_then(|path| path.strip_prefix("/dev").ok().map(Path::to_path_buf))
            .unwrap_or(device_path)
            .to_str()
            .map(str::to_owned)
            .unwrap_or_default()
    }

    fn parse_diskstats(content: &str) -> HashMap<String, LinuxDiskIoOperations> {
        let mut stats = HashMap::new();
        for line in content.lines() {
            let mut iter = line.split_whitespace();
            let Some(name) = iter.nth(2) else {
                continue;
            };
            let read_operations = iter
                .next()
                .and_then(|value| u64::from_str(value).ok())
                .unwrap_or(0);
            let write_operations = iter
                .nth(3)
                .and_then(|value| u64::from_str(value).ok())
                .unwrap_or(0);
            stats.insert(
                name.to_owned(),
                LinuxDiskIoOperations {
                    read_operations,
                    write_operations,
                },
            );
        }
        stats
    }

    let diskstats = std::fs::read_to_string("/proc/diskstats").ok()?;
    let diskstats = parse_diskstats(&diskstats);
    let mut total = SystemDiskIoOperations::default();
    let mut found_disk = false;
    for disk in disks.list() {
        let device_name = actual_device_name(disk.name());
        let Some(operations) = diskstats.get(&device_name) else {
            continue;
        };
        total.read_operations = total
            .read_operations
            .saturating_add(operations.read_operations);
        total.write_operations = total
            .write_operations
            .saturating_add(operations.write_operations);
        found_disk = true;
    }

    found_disk.then_some(total)
}

#[cfg(target_os = "macos")]
fn collect_macos_disk_io_operations(disks: &sysinfo::Disks) -> Option<SystemDiskIoOperations> {
    use std::ffi::CStr;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr;

    type CfAllocatorRef = *const libc::c_void;
    type CfDictionaryRef = *const libc::c_void;
    type CfNumberRef = *const libc::c_void;
    type CfStringRef = *const libc::c_void;
    type CfTypeRef = *const libc::c_void;
    type IoIteratorT = libc::mach_port_t;
    type IoObjectT = libc::mach_port_t;
    type IoRegistryEntryT = libc::mach_port_t;

    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_SINT64_TYPE: libc::c_int = 4;
    const IO_SERVICE_PLANE: &[u8] = b"IOService\0";
    const IO_BLOCK_STORAGE_DRIVER: &[u8] = b"IOBlockStorageDriver\0";

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: CfAllocatorRef,
            c_str: *const libc::c_char,
            encoding: u32,
        ) -> CfStringRef;
        fn CFRelease(cf: CfTypeRef);
        fn CFDictionaryGetValueIfPresent(
            dict: CfDictionaryRef,
            key: *const libc::c_void,
            value: *mut *const libc::c_void,
        ) -> libc::c_uchar;
        fn CFNumberGetValue(
            number: CfNumberRef,
            number_type: libc::c_int,
            value: *mut libc::c_void,
        ) -> libc::c_uchar;
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOBSDNameMatching(
            main_port: libc::mach_port_t,
            options: u32,
            bsd_name: *const libc::c_char,
        ) -> CfDictionaryRef;
        fn IOServiceGetMatchingServices(
            main_port: libc::mach_port_t,
            matching: CfDictionaryRef,
            existing: *mut IoIteratorT,
        ) -> libc::kern_return_t;
        fn IOIteratorNext(iterator: IoIteratorT) -> IoObjectT;
        fn IOObjectRelease(object: IoObjectT) -> libc::kern_return_t;
        fn IORegistryEntryGetParentEntry(
            entry: IoRegistryEntryT,
            plane: *const libc::c_char,
            parent: *mut IoRegistryEntryT,
        ) -> libc::kern_return_t;
        fn IORegistryEntryCreateCFProperty(
            entry: IoRegistryEntryT,
            key: CfStringRef,
            allocator: CfAllocatorRef,
            options: u32,
        ) -> CfTypeRef;
        fn IOObjectConformsTo(
            object: IoObjectT,
            class_name: *const libc::c_char,
        ) -> libc::c_uchar;
    }

    struct CfString {
        inner: CfStringRef,
    }

    impl CfString {
        fn new(value: &str) -> Option<Self> {
            let value = CString::new(value).ok()?;
            let inner = unsafe {
                CFStringCreateWithCString(ptr::null(), value.as_ptr(), CF_STRING_ENCODING_UTF8)
            };
            if inner.is_null() {
                None
            } else {
                Some(Self { inner })
            }
        }

        fn as_ptr(&self) -> CfStringRef {
            self.inner
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.inner);
            }
        }
    }

    struct CfType {
        inner: CfTypeRef,
    }

    impl CfType {
        fn new(inner: CfTypeRef) -> Option<Self> {
            if inner.is_null() {
                None
            } else {
                Some(Self { inner })
            }
        }

        fn as_dictionary(&self) -> CfDictionaryRef {
            self.inner
        }
    }

    impl Drop for CfType {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.inner);
            }
        }
    }

    struct IoObject {
        inner: IoObjectT,
    }

    impl IoObject {
        fn new(inner: IoObjectT) -> Option<Self> {
            if inner == 0 {
                None
            } else {
                Some(Self { inner })
            }
        }

        fn into_inner(self) -> IoObjectT {
            let inner = self.inner;
            std::mem::forget(self);
            inner
        }
    }

    impl Drop for IoObject {
        fn drop(&mut self) {
            unsafe {
                IOObjectRelease(self.inner);
            }
        }
    }

    fn bsd_name_for_mount_point(mount_point: &Path) -> Option<CString> {
        let mount_point = CString::new(mount_point.as_os_str().as_bytes()).ok()?;
        let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::statfs(mount_point.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        let mounted_from = unsafe { CStr::from_ptr(stat.f_mntfromname.as_ptr()) }.to_bytes();
        let bsd_name = mounted_from
            .strip_prefix(b"/dev/")
            .unwrap_or(mounted_from);
        CString::new(bsd_name).ok()
    }

    fn dictionary_u64(dict: CfDictionaryRef, key: CfStringRef) -> Option<u64> {
        let mut value = ptr::null();
        if unsafe { CFDictionaryGetValueIfPresent(dict, key.cast(), &mut value) } == 0 {
            return None;
        }
        if value.is_null() {
            return None;
        }

        let mut number = 0_i64;
        if unsafe {
            CFNumberGetValue(
                value.cast::<libc::c_void>(),
                CF_NUMBER_SINT64_TYPE,
                &mut number as *mut i64 as *mut libc::c_void,
            )
        } == 0
        {
            return None;
        }

        Some(number.max(0) as u64)
    }

    fn operations_for_bsd_name(
        bsd_name: &CStr,
        stats_key: CfStringRef,
        read_key: CfStringRef,
        write_key: CfStringRef,
    ) -> Option<SystemDiskIoOperations> {
        let matching = unsafe { IOBSDNameMatching(0, 0, bsd_name.as_ptr()) };
        if matching.is_null() {
            return None;
        }

        let mut iterator = 0;
        if unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) }
            != libc::KERN_SUCCESS
        {
            return None;
        }
        let iterator = IoObject::new(iterator)?;

        while let Some(service) = IoObject::new(unsafe { IOIteratorNext(iterator.inner) }) {
            let mut current = service.into_inner();
            loop {
                let mut parent = 0;
                if unsafe {
                    IORegistryEntryGetParentEntry(
                        current,
                        IO_SERVICE_PLANE.as_ptr().cast(),
                        &mut parent,
                    )
                } != libc::KERN_SUCCESS
                {
                    unsafe {
                        IOObjectRelease(current);
                    }
                    break;
                }

                unsafe {
                    IOObjectRelease(current);
                }
                current = parent;

                let Some(stats) = CfType::new(unsafe {
                    IORegistryEntryCreateCFProperty(current, stats_key, ptr::null(), 0)
                }) else {
                    continue;
                };

                if unsafe {
                    IOObjectConformsTo(current, IO_BLOCK_STORAGE_DRIVER.as_ptr().cast())
                } == 0
                {
                    continue;
                }

                if let (Some(read_operations), Some(write_operations)) = (
                    dictionary_u64(stats.as_dictionary(), read_key),
                    dictionary_u64(stats.as_dictionary(), write_key),
                ) {
                    unsafe {
                        IOObjectRelease(current);
                    }
                    return Some(SystemDiskIoOperations {
                        read_operations,
                        write_operations,
                    });
                }
            }
        }

        None
    }

    let stats_key = CfString::new("Statistics")?;
    let read_key = CfString::new("Operations (Read)")?;
    let write_key = CfString::new("Operations (Write)")?;

    let mut total = SystemDiskIoOperations::default();
    let mut found_disk = false;
    for disk in disks.list() {
        let Some(bsd_name) = bsd_name_for_mount_point(disk.mount_point()) else {
            continue;
        };
        let Some(operations) = operations_for_bsd_name(
            bsd_name.as_c_str(),
            stats_key.as_ptr(),
            read_key.as_ptr(),
            write_key.as_ptr(),
        ) else {
            continue;
        };
        total.read_operations = total
            .read_operations
            .saturating_add(operations.read_operations);
        total.write_operations = total
            .write_operations
            .saturating_add(operations.write_operations);
        found_disk = true;
    }

    found_disk.then_some(total)
}

#[derive(Debug)]
struct SystemCpuUsageSample {
    timestamp: Instant,
    usage: HostCpuUsage,
}

pub fn system_cpu_usage() -> Option<f64> {
    static PREVIOUS: OnceLock<Mutex<Option<SystemCpuUsageSample>>> = OnceLock::new();

    let current = SystemCpuUsageSample {
        timestamp: Instant::now(),
        usage: HostCpuUsage::get().ok()?,
    };

    let mut previous = PREVIOUS.get_or_init(|| Mutex::new(None)).lock().ok()?;
    let previous = previous.replace(SystemCpuUsageSample {
        timestamp: current.timestamp,
        usage: current.usage.clone(),
    })?;

    system_cpu_usage_from_samples(&previous, &current)
}

fn system_cpu_usage_from_samples(
    previous: &SystemCpuUsageSample,
    current: &SystemCpuUsageSample,
) -> Option<f64> {
    let elapsed = current
        .timestamp
        .checked_duration_since(previous.timestamp)?;
    let elapsed_millis = duration_millis(elapsed)?;
    if elapsed_millis == 0 {
        return None;
    }

    let user_millis = current
        .usage
        .user_millis
        .checked_sub(previous.usage.user_millis)?;
    let system_millis = current
        .usage
        .system_millis
        .checked_sub(previous.usage.system_millis)?;
    let busy_millis = user_millis.checked_add(system_millis)?;

    Some(busy_millis as f64 / elapsed_millis as f64)
}

fn duration_millis(duration: Duration) -> Option<u64> {
    duration.as_millis().try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::SystemCpuUsageSample;
    use super::SystemDiskIoOperations;
    use super::bytes_per_second;
    use super::operations_per_second;
    use super::system_cpu_usage_from_samples;
    use super::system_memory_stats;
    use super::system_memory_stats_detailed;
    use crate::os::host_cpu_usage::HostCpuUsage;
    use std::time::Duration;
    use std::time::Instant;

    #[test]
    fn get_system_memory_stats() {
        let total_mem = system_memory_stats();
        // sysinfo returns zero when fails to retrieve data
        assert!(total_mem > 0);
    }

    #[test]
    fn get_detailed_system_memory_stats() {
        let memory = system_memory_stats_detailed();
        // sysinfo returns zero when fails to retrieve data
        assert!(memory.total > 0);
        assert!(memory.available > 0);
    }

    #[test]
    fn get_system_cpu_usage_from_samples() {
        let start = Instant::now();
        let previous = SystemCpuUsageSample {
            timestamp: start,
            usage: HostCpuUsage {
                user_millis: 1000,
                system_millis: 500,
            },
        };
        let current = SystemCpuUsageSample {
            timestamp: start + Duration::from_millis(100),
            usage: HostCpuUsage {
                user_millis: 1250,
                system_millis: 650,
            },
        };

        assert_eq!(
            system_cpu_usage_from_samples(&previous, &current),
            Some(4.0)
        );
    }

    #[test]
    fn test_bytes_per_second() {
        assert_eq!(bytes_per_second(1024, Duration::from_millis(500)), 2048);
        assert_eq!(bytes_per_second(1024, Duration::ZERO), 0);
    }

    #[test]
    fn test_operations_per_second() {
        let previous = SystemDiskIoOperations {
            read_operations: 10,
            write_operations: 20,
        };
        let current = SystemDiskIoOperations {
            read_operations: 15,
            write_operations: 30,
        };
        assert_eq!(
            operations_per_second(
                Some(previous),
                Some(current),
                Duration::from_millis(500),
                |operations| operations.read_operations,
            ),
            Some(10)
        );
        assert_eq!(
            operations_per_second(
                None,
                Some(current),
                Duration::from_millis(500),
                |operations| operations.read_operations,
            ),
            None
        );
    }
}
