/*
 * Copyright (c) 2021, 2023-2024 Qualcomm Innovation Center, Inc. All rights reserved.
 * SPDX-License-Identifier: BSD-3-Clause-Clear
 */

mod panic_hook;

use std::env;
use std::default::Default;
use std::path::{Path, PathBuf};
use std::string::String;
use std::fs;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{RawFd, FromRawFd};
use std::thread;
use std::io;
use std::fmt::{self, Display};
use std::str::FromStr;
use std::thread::JoinHandle;
use std::process;
use std::net;
use std::thread::sleep;
use std::time::Duration;
use net_util::{MacAddress, Tap, TapT};
extern crate simplelog;
use simplelog::*;

extern crate android_logger;
use libc::{self, c_uint, c_int, c_char, open, O_RDWR, O_WRONLY};

use devices::virtio::input::{constants::*, new_evdev};
use devices::virtio::{self, base_features, Block, Console, Net};
use devices::serial_device::{SerialHardware, SerialParameters, SerialType};
use hypervisor::{ProtectionType};
use mmio::MmioDevice;
use mmio::DEVICE_RESET;
use devices::virtio::vhost::{Scmi, Vsock, vsock::VhostVsockConfig};

use base::{pagesize, AsRawDescriptor};
use base::{info, error, debug, Event, RawDescriptor, syslog};
use vm_memory::{GuestAddress, GuestMemory, GuestMemoryError, MemoryRegion};
use std::sync::Arc;
use std::convert::TryInto;

use devices::virtio::block::block::DiskOption;
#[cfg(not(feature = "vhost-user-generic"))]
use devices::virtio::vhost::user::vmm::{
    Hab as VhostUserHab, Scmi as VhostUserScmi, I2cAdapter as VhostUserI2cAdapter,
    GlinkPassthrough as VhostUserGP, Frpc as VhostUserfrpc, Ssr as VhostUserSsr,
    Eavb as VhostUserEAVB, Fs as VhostUserfs,  Gpio as VhostUserGpio,
};
#[cfg(feature = "vhost-user-generic")]
use devices::virtio::vhost::user::vmm::{
    Hab as VhostUserHab, Scmi as VhostUserScmi, I2cAdapter as VhostUserI2cAdapter,
    GlinkPassthrough as VhostUserGP, Frpc as VhostUserfrpc, Ssr as VhostUserSsr,
    Eavb as VhostUserEAVB, Fs as VhostUserfs, GenericDevice as VhostUserGeneric,
    Gpio as VhostUserGpio,
};

use crosvm::{
    argument::{self, set_arguments, Argument},VhostUserOption,
};
use base::{FlockOperation, validate_raw_fd, flock};
use base::{ioctl_with_val, ioctl_io_nr, ioctl_with_ref, ioctl_with_mut_ref, ioctl_iow_nr, ioctl_ior_nr, ioctl_iowr_nr, SafeDescriptor, FromRawDescriptor};

use vhost::NetT;
use virtio_sys;
static VHOST_NET_PATH: &str = "/dev/vhost-net";
static DEF_SERIAL_FILE: &str = "/tmp/la_gvm.txt";
static VSOCK_PATH: &str = "/dev/vhost-vsock";

// Logging
#[macro_use]
extern crate log;
use log::{Level, LevelFilter};
use android_logger::{Config};

// Minijail
use minijail::Minijail;
static GH_PATH: &str = "/dev/gunyah";
static VIRTIO_BE_PATH: &str = "/dev/gh_virtio_backend_";
static TRACE_MARKER: &str = "/sys/kernel/tracing/trace_marker";
static VHOST_SCMI_PATH: &str = "/dev/vhost-scmi";

// Todo: Use UAPI header files
const ASSIGN_EVENTFD: u32 = 1;
const GH_IOCTL_TYPE_V2: u32 = 0xB2;
const GH_IOCTL_TYPE_V1: u32 = 0xBC;

const VBE_ASSIGN_IRQFD: u32 = 1;

const EVENT_RESET_RQST: u32 = 2;
const EVENT_INTERRUPT_ACK: u32 = 4;
const EVENT_DRIVER_OK: u32 = 8;
const EVENT_APP_EXIT: u32 = 0x100;

const VIRTIO_MMIO_DEVICE_FEATURES: u64 = 0x10;
const VIRTIO_MMIO_DEVICE_FEATURES_SEL: u64 = 0x14;
const VIRTIO_MMIO_DRIVER_FEATURES: u64 = 0x20;
const VIRTIO_MMIO_DRIVER_FEATURES_SEL: u64 = 0x24;
const VIRTIO_MMIO_QUEUE_SEL: u64 = 0x30;
const VIRTIO_MMIO_QUEUE_NUM_MAX: u64 = 0x34;
const VIRTIO_MMIO_QUEUE_NUM: u64 = 0x38;
const VIRTIO_MMIO_QUEUE_READY: u64 = 0x44;
const VIRTIO_MMIO_INTERRUPT_ACK: u64 = 0x64;
const VIRTIO_MMIO_QUEUE_DESC_LOW: u64 = 0x80;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: u64 = 0x84;
const VIRTIO_MMIO_QUEUE_AVAIL_LOW: u64 = 0x90;
const VIRTIO_MMIO_QUEUE_AVAIL_HIGH: u64 = 0x94;
const VIRTIO_MMIO_QUEUE_USED_LOW: u64 = 0xa0;
const VIRTIO_MMIO_QUEUE_USED_HIGH: u64 = 0xa4;
const VIRTIO_MMIO_STATUS: u64 = 0x70;
const VIRTIO_MMIO_STATUS_IDX: u64 = 28;
const VIRTIO_MMIO_INPUT_SEL: u64 = 0x100;
const VIRTIO_MMIO_DEVICE_CONFIG: u64 = 0x100;

const GH_VCPU_MAX: u16 = 512;
const CROSVM_MINIJAIL_POLICY: &str = "/system_ext/etc/seccomp_policy/qcrosvm.policy";
const LOG_TAG: &str = "qcrosvm";
const RETRY_LIMIT: u16 = 20;
const RETRY_DELAY_MS: u64 = 100;

#[derive(Debug)]
enum BackendError {
    StrError(String),
    StrNumError{err: String, val: io::Error},
}

impl Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::BackendError::*;

        match self {
            StrError(s) => write!(f, "{}", format!("Error: {}", s)),
            StrNumError{err, val} => write!(f, "{}", format!("Error: {} ({})", err, val)),
        }
    }
}

macro_rules! create_device_threads {
    ($self:expr, $cfg:expr, $devices:expr, $create_devices:expr, $init_fn:expr) => {{

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        if !$devices.is_empty() {

            // create devices call
            let e = $create_devices($self, $cfg);
            if let Err(_e) = e {
                error!("{}", _e);
                panic!("{}", _e);
            }

            for device in $devices {
                let label = device.label;
                let mut sfd = $cfg.vm_sfd.as_mut().expect(&format!("{}:{}", file!(), line!())).try_clone()
                    .expect(&format!("{}:{}", file!(), line!()));
                let mut mmio = device.mmio.take().expect(&format!("{}:{}", file!(), line!()));
                let mut cspace = device.config_space.take().expect(&format!("{}:{}", file!(), line!()));
                let driver_variant = $cfg.driver_variant;
                $init_fn(&mut cspace, label, &mut mmio, &mut sfd, driver_variant);

                debug!("Thread being created for device with label: {}", label);
                let handle = std::thread::spawn(move || {
                    handle_events(label, sfd, &mut mmio, &mut cspace, driver_variant);
                });
                handles.push(handle);
            }
        }
        handles
    }};
}

fn mmio_handle(mmio: &Option<MmioDevice>, label: u32, sfd: &SafeDescriptor, cfg: &BackendConfig) -> Result<(), BackendError> {

    let mmio = mmio.as_ref().expect(&format!("{}:{}", file!(), line!()));
    let mut idx = 0;

    for e in mmio.queue_evts() {
        let event_fd = VirtioEventfd {
            _label : label,
            _flags : ASSIGN_EVENTFD,
            _queue_num : idx,
            _fd : e.as_raw_descriptor(),
        };

        idx = idx + 1;
        let ret = unsafe { ioctl_with_ref(sfd, to_cmd(VmIoctl::IoEventFd, cfg.driver_variant)
                                            .expect(&format!("{}:{}", file!(), line!())), &event_fd) };
        if ret < 0 {
            return Err(BackendError::StrNumError {
                err: String::from("ioeventfd ioctl failed"),
                val: io::Error::last_os_error(),
            });
        }
    }

    let irq_fd = VirtioIrqfd {
        _label: label,
        _fd : mmio.interrupt_evt().expect(&format!("{}:{}", file!(), line!())).as_raw_descriptor(),
        _flags: VBE_ASSIGN_IRQFD,
        _reserved: 0,
    };

    let ret = unsafe { ioctl_with_ref(sfd, to_cmd(VmIoctl::IrqFd, cfg.driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &irq_fd) };
    if ret < 0 {
        return Err(BackendError::StrNumError {
            err: String::from("irqfd ioctl failed"),
            val: io::Error::last_os_error(),
        });
    }

    Ok(())
}

trait DeviceTrait {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()>;
    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()>;
}

struct VirtioDisk {
    disk: DiskOption,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}

impl VirtioDisk {
    pub fn new() -> Self {
        Self {
            disk: DiskOption {
                path: PathBuf::new(),
                read_only: true, /*mount read only - default*/
                o_direct: false, /*Use O_DIRECT mode to bypass page cache. (default: false)*/
                sparse: true,
                block_size: 512,
                id: None,
            },
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
        }
    }

    fn create_bdev(&mut self, q_size: Option<u16>) -> Result<Box<Block>, BackendError> {
        // Special case '/proc/self/fd/*' paths. The FD is already open, just use it.
        let raw_image: File = if self.disk.path.parent() == Some(Path::new("/proc/self/fd")) {
            // Safe because we will validate |raw_fd|.
            unsafe {File::from_raw_fd(raw_fd_from_path(&self.disk.path).map_err(|_| BackendError::StrError(String::from("raw_fd_from_path failed")))?)}
        } else {
            let mut options = OpenOptions::new();
            options.read(true).write(!self.disk.read_only);
            if self.disk.o_direct {
              options.custom_flags(libc::O_DIRECT);
            }
            options.open(&self.disk.path).map_err(|_| BackendError::StrNumError {
              err: String::from("open of disk file failed"),
              val: io::Error::last_os_error(),
            })?
        };

        // Lock the disk image to prevent other crosvm instances from using it.
        let lock_op = if self.disk.read_only {
            FlockOperation::LockShared
        } else {
            FlockOperation::LockExclusive
        };

        flock(&raw_image, lock_op, true).map_err(|_| BackendError::StrNumError {
            err: String::from("flock on disk file failed"),
            val: io::Error::last_os_error(),
        })?;

        let disk_file = disk::create_disk_file(raw_image, disk::MAX_NESTING_DEPTH, Path::new(&self.disk.path)).map_err(|_| BackendError::StrNumError {
            err: String::from("create_disk_file failed"),
            val: io::Error::last_os_error(),
        })?;

        let dev = virtio::Block::new(
            base_features(ProtectionType::Unprotected) ,
            disk_file ,
            self.disk.read_only,
            self.disk.sparse,
            self.disk.block_size,
            None,
            None,
            q_size,
            ).map_err(|_| BackendError::StrError(String::from("virtio_block_new failed")))?;

        Ok(Box::new(dev))
    }
}

struct VirtioDiskDevices {
    virtio_disks: Vec<VirtioDisk>,
}

impl VirtioDiskDevices {
    pub fn new() -> Self {
        VirtioDiskDevices { virtio_disks: Vec::new() }
    }

    fn create_block_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vdisk in &mut self.virtio_disks {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;
            let q_size :Option<u16>;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!())); q_size = Some(256)}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!())); q_size = Some(128)}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let bdev = vdisk.create_bdev(q_size)?;

            vdisk.mmio = Some(MmioDevice::new(mem.clone(), bdev).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vdisk.mmio, vdisk.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioDiskDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.virtio_disks,
            VirtioDiskDevices::create_block_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vdisk = VirtioDisk::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vdisk.disk.path =
            PathBuf::from(
                components
                .next()
                .ok_or_else(|| argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("disk path must be provided"),
                })?
            );

        if !vdisk.disk.path.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("disk path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("disk options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("disk options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vdisk.label = label;
                }
                "sparse" => {
                    let sparse = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("`sparse` must be a boolean"),
                    })?;
                    vdisk.disk.sparse = sparse;
                }
                "block_size" => {
                    let block_size =
                        value.parse().map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`block_size` must be an integer"),
                        })?;
                    match block_size {
                        512 | 1024 => vdisk.disk.block_size = block_size,
                        _ => {
                            return Err(argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`block_size` must be 512 or 1024"),
                            });
                        }
                    }
                }
                "rw" => {
                    let rwrite: bool = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("`rw` must be a boolean"),
                    })?;
                    vdisk.disk.read_only = !rwrite;
                }
                "dio" => {
                  let direct_io: bool = value.parse().map_err(|_| argument::Error::InvalidValue {
                      value: value.to_owned(),
                      expected: String::from("`dio` must be a boolean"),
                  })?;
                  vdisk.disk.o_direct = direct_io;
              }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported disk options only"),
                    });
                }
            }
        }

        self.virtio_disks.push(vdisk);
        Ok(())
    }
}

///// VIRTIO_NET //////
pub struct VirtioNet {
    ip_addr: Option<net::Ipv4Addr>,
    netmask: Option<net::Ipv4Addr>,
    mac_addr: Option<MacAddress>,
    tap_name: Option<String>,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}

impl VirtioNet {
    pub fn new() -> Self {
        Self {
            ip_addr: None,
            netmask: None,
            mac_addr: None,
            tap_name: None,
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
        }
    }
}

pub struct VirtioNetDevices {
    virtio_nets: Vec<VirtioNet>,
    network_dev: bool,
    vhost_net_device_path: PathBuf,
    vhost_net: bool,
}

impl VirtioNetDevices {
    pub fn new() -> Self {
        VirtioNetDevices {
            virtio_nets: Vec::new(),
            network_dev: false,
            vhost_net_device_path: PathBuf::from(VHOST_NET_PATH),
            vhost_net: true,
        }
    }

    fn create_net_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {
        let mut name : &[u8] = b"vmtap%d";

        for vnet in &mut self.virtio_nets {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            if vnet.ip_addr.is_some() || vnet.netmask.is_some() || vnet.mac_addr.is_some() {
                if vnet.ip_addr.is_none() {
                    println!("ip address not found");
                }
                if vnet.netmask.is_none() {
                    println!("netmask not found");
                }
                if vnet.mac_addr.is_none() {
                    println!("mac address not found");
                }
            }

            if let (Some(ip_addr), Some(netmask), Some(mac_addr)) = (vnet.ip_addr, vnet.netmask, vnet.mac_addr) {
                if self.vhost_net {
                    let mut ndev;
                    if vnet.tap_name.is_some() {
                        let vmname_string: &String = &vnet.tap_name.as_ref().unwrap();
                        let str_name = b"vmtap-";
                        let name  = &[str_name,vmname_string.as_bytes()].concat();
                        ndev = virtio::vhost::Net::<Tap, vhost::Net<Tap>>::new_with_name(
                            &self.vhost_net_device_path,
                            base_features(ProtectionType::Unprotected),
                            ip_addr,
                            netmask,
                            mac_addr,
                            name,
                            ).map_err(|_| BackendError::StrError(String::from("new with name failed failed")))?;
                    }
                    else
                    {
                        ndev = virtio::vhost::Net::<Tap, vhost::Net<Tap>>::new(
                            &self.vhost_net_device_path,
                            base_features(ProtectionType::Unprotected),
                            ip_addr,
                            netmask,
                            mac_addr,
                            ).map_err(|_| BackendError::StrError(String::from("vhost_net_new failed")))?;
                    }

                    vnet.mmio = Some(MmioDevice::new(mem.clone(), Box::new(ndev)).expect(&format!("{}:{}", file!(), line!())));
                    mmio_handle(&vnet.mmio, vnet.label, sfd, cfg)?;
                }
            }
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioNetDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.virtio_nets,
            VirtioNetDevices::create_net_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vnet_dev = VirtioNet::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        if let Some("true") = components.next() {
            self.network_dev = true;
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("net options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("net options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("invalid `label` value"),
                        });
                    }
                    vnet_dev.label = label;
                }
                "ip_addr" => {
                    vnet_dev.ip_addr =
                        Some(
                            value
                            .parse()
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`ip_addr` needs to be in the form \"x.x.x.x\""),
                            })?,
                            );
                }
                "netmask" => {
                    vnet_dev.netmask =
                        Some(
                            value
                            .parse()
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`netmask` needs to be in the form \"x.x.x.x\""),
                            })?,
                            );
                }
                "mac" => {
                    vnet_dev.mac_addr =
                        Some(
                            value
                            .parse()
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from(
                                    "`mac` needs to be in the form \"XX:XX:XX:XX:XX:XX\"",
                                    ),
                            })?,
                            );
                }
                "tapName" => {
                    vnet_dev.tap_name =
                        Some(
                            value
                            .parse()
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from(
                                    "vm_name expected",
                                    ),
                            })?,
                            );
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("unrecognized net option"),
                    });
                }
            }
        }

        self.virtio_nets.push(vnet_dev);
        Ok(())
    }
}

///// VU_VIRTIO_GP /////
pub struct VuVirtioGP {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_gp: VhostUserOption,
}

impl VuVirtioGP {
    pub fn new() -> Self {
        VuVirtioGP {
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_gp: VhostUserOption {
                socket: PathBuf::new()
            }
        }
    }
}

pub struct VuGPDevices {
    vugp_devices: Vec<VuVirtioGP>,
}
impl VuGPDevices {
    pub fn new() -> Self {
        VuGPDevices {vugp_devices: Vec::new() }
    }
    fn create_vugp_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vugp in &mut self.vugp_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vugpdev =  VhostUserGP::new(virtio::base_features(ProtectionType::Unprotected), &vugp.vhost_user_gp.socket)
                .map_err(|_| BackendError::StrError(String::from("vhost gp new failed")))?;

            vugp.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vugpdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vugp.mmio, vugp.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VuGPDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vugp_devices,
            VuGPDevices::create_vugp_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vugp = VuVirtioGP::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vugp.vhost_user_gp = VhostUserOption {
            socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-gp socket path must be provided"),
                        })?,
                        ),
        };

        if !vugp.vhost_user_gp.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-gp socket path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost gp options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost gp options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be an unsigned integer"),
                                })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                            });
                        }
                    vugp.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported vhost gp options only"),
                    });
                }
            }
        }

        self.vugp_devices.push(vugp);
        Ok(())
    }
}

////// VIRTIO_INPUT //////
pub struct VirtioInput {
    dev_path: PathBuf,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}

impl VirtioInput {
    pub fn new() -> Self {
        Self {
            dev_path: PathBuf::new(),
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
        }
    }
}

pub struct VirtioInputDevices {
    virtio_inputs: Vec<VirtioInput>,
}

impl VirtioInputDevices {
    pub fn new() -> Self {
        VirtioInputDevices{ virtio_inputs: Vec::new() }
    }

    fn create_vinput_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vinput in &mut self.virtio_inputs {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };

            let dev_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&vinput.dev_path)
                .map_err(|_| BackendError::StrNumError {
                    err: String::from("open vinput device faild"),
                    val: io::Error::last_os_error(),
                })?;

            let inputdev = virtio::new_evdev(dev_file, base_features(ProtectionType::Unprotected))
                .map_err(|_| BackendError::StrError(String::from("set up input device failed")))?;

            vinput.mmio = Some(MmioDevice::new(mem.clone(), Box::new(inputdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vinput.mmio, vinput.label, sfd, cfg)?;

            // Call input init config
            match &mut vinput.mmio {
                Some(mmio) => {
                    let mut sfd = cfg.vm_sfd.as_mut().expect(&format!("{}:{}", file!(), line!())).try_clone()
                    .expect(&format!("{}:{}", file!(), line!()));

                    init_input_config(vinput.label, mmio, &mut sfd, cfg.driver_variant);
                },
                None => {
                    return Err(BackendError::StrError(String::from("None mmio!")));
                },
            }
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioInputDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.virtio_inputs,
            VirtioInputDevices::create_vinput_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vinput = VirtioInput::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        let input_dev_path = PathBuf::from(
                components
                .next()
                .ok_or_else(|| argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("input device path must be provided"),
                })?
                );

        let path = Path::new(&input_dev_path);

        if path.exists() {
            vinput.dev_path = input_dev_path;

            for opt in components {
                let mut o = opt.splitn(2, '=');
                let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("input options must not be empty"),
                })?;
                let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("input options must be of the form `kind=value`"),
                })?;

                match kind {
                    "label" => {
                        let label: u32 = u32::from_str_radix(value, 16)
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be an unsigned integer"),
                            })?;
                        if label == 0 {
                            return Err(argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be a non zero integer"),
                            });
                        }
                        vinput.label = label;
                    }
                    _ => {
                        return Err(argument::Error::InvalidValue {
                            value: kind.to_owned(),
                            expected: String::from("supported input options only"),
                        });
                    }
                }
            }
            self.virtio_inputs.push(vinput);
        } else {
            println!("Warning: The input device path does not exist.");
        }

        Ok(())
    }
}

////// VIRTIO_HAB //////
pub struct VirtioHab {
    label: u32,
    hab_device_id : u32,
    no_of_queues : u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_hab: VhostUserOption,
}

impl VirtioHab {
    pub fn new() -> Self {
        Self {
            label: 0,
            hab_device_id : 0,
            no_of_queues : 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_hab: VhostUserOption {
                socket: PathBuf::new()
            },
        }
    }
}

pub struct VirtioHabDevices {
    virtio_hab_devices: Vec<VirtioHab>,
}

impl VirtioHabDevices {
    pub fn new() -> Self {
        VirtioHabDevices { virtio_hab_devices: Vec::new() }
    }

    fn create_vhab_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for hab in &mut self.virtio_hab_devices {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let habdev =  VhostUserHab::new(virtio::base_features(ProtectionType::Unprotected), &hab.vhost_user_hab.socket, hab.hab_device_id,  hab.no_of_queues)
                .map_err(|_| BackendError::StrError(String::from("vhost hab new failed")))?;

            hab.mmio = Some(MmioDevice::new(mem.clone(), Box::new(habdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&hab.mmio, hab.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioHabDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.virtio_hab_devices,
            VirtioHabDevices::create_vhab_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vhab = VirtioHab::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');
        let mut retries = 0;
        let max_retries = RETRY_LIMIT;

        vhab.vhost_user_hab = VhostUserOption {
            socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-hab socket path must be provided"),
                        })?,
                        ),
        };

        loop {
            if vhab.vhost_user_hab.socket.exists() {
                break;
            }
            retries += 1;
            if retries >= max_retries {
                return Err(argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("vhost-user-hab socket path must be an existing path"),
                });
            }
            sleep(Duration::from_millis(RETRY_DELAY_MS));
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost HAB options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost HAB options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vhab.label = label;
                }
                "device-id" => {
                    vhab.hab_device_id = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("device-id must be an integer"),
                    })?;
                }
                "queue-num" => {
                    vhab.no_of_queues = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("queue number must be an integer "),
                    })?;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported vhost hab options only"),
                    });
                }
            }
        }

        self.virtio_hab_devices.push(vhab);
        Ok(())
    }
}

////// VIRTIO_CONSOLE //////
pub struct VirtioConsole {
    serial_params: SerialParameters,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}

impl VirtioConsole {
    pub fn new() -> Self {
        VirtioConsole {
            serial_params: SerialParameters {
                type_: SerialType::Stdout,
                hardware: SerialHardware::VirtioConsole,
                path: None,
                input: None,
                num: 1,
                console: false,
                earlycon: false,
                stdin: false,
                out_timestamp: false,
            },
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
        }
    }
}

pub struct VirtioConsoleDevices {
    v_consoles: Vec<VirtioConsole>,
}

impl VirtioConsoleDevices {
    pub fn new() -> Self {
        VirtioConsoleDevices { v_consoles: Vec::new() }
    }

    fn create_console_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vconsole in &mut self.v_consoles {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };

            let mut keep_rds = Vec::new();
            let evt = Event::new().map_err(|_| BackendError::StrError(String::from("failed to create event")))?;
            let params = &vconsole.serial_params;
            let condev = params
                .create_serial_device::<Console>(ProtectionType::Unprotected, &evt, &mut keep_rds)
                .map_err(|_| BackendError::StrError(String::from("failed to create console device")))?;

            vconsole.mmio = Some(MmioDevice::new(mem.clone(), Box::new(condev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vconsole.mmio, vconsole.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioConsoleDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.v_consoles,
            VirtioConsoleDevices::create_console_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vconsole = VirtioConsole::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        let serial_file =
            PathBuf::from(
            components
            .next()
            .ok_or_else(|| argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("console backend file path must be provided"),
            })?
        );

        let serial_type;
        let serial_path;
        let serial_stdin;

        if serial_file.ends_with("stdio") {
            println!("Serial Type: Stdout");
            serial_type = SerialType::Stdout;
            serial_path = None;
            serial_stdin = true;
        } else {
            println!("Serial Type: File");
            serial_type = SerialType::File;
            serial_stdin = false;
            let mut current_path;
            if !serial_file.has_root() {
                current_path = env::current_dir().unwrap();
                current_path.push(serial_file);
            } else {
                current_path = serial_file;
            }
            println!("The expected serial file is {}", current_path.display());
            // Check if able to write inside directory
            let res = File::options()
                .write(true)
                .create(true)
                .open(&current_path);

            if res.is_ok() {
                serial_path = Some(current_path);
            } else {
                println!("But the directory is Read-Only, so take default serial file {}.", DEF_SERIAL_FILE);
                serial_path = Some(PathBuf::from(DEF_SERIAL_FILE));
            }

            if let Some(log_file) = serial_path.as_ref() {
                if log_file.exists() {
                    println!("Remove previous log file {}", log_file.to_string_lossy());
                    fs::remove_file(log_file);
                }
            }
        }
        // Add a virtio-console device with console=true.
        vconsole.serial_params = SerialParameters {
            type_: serial_type,
            hardware: SerialHardware::VirtioConsole,
            path: serial_path,
            input: None,
            num: 1,
            console: true,
            earlycon: false,
            stdin: serial_stdin,
            out_timestamp: false,
        };

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("console options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("console options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                    })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vconsole.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported console options only"),
                    });
                }
            }
        }

        self.v_consoles.push(vconsole);
        Ok(())
    }
}

////// VU_VIRTIO_SCMI //////
pub struct VuVirtioScmi {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_scmi: VhostUserOption,
}

impl VuVirtioScmi {
    pub fn new() -> Self {
        VuVirtioScmi{
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_scmi: VhostUserOption {
                socket: PathBuf::new(),
            },
        }
    }
}

pub struct VuScmiDevices {
    vuscmi_devices: Vec<VuVirtioScmi>,
}

impl VuScmiDevices {
    pub fn new() -> Self {
        VuScmiDevices {vuscmi_devices: Vec::new()}
    }

    fn create_vuscmi_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vuscmi in &mut self.vuscmi_devices {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vuscmidev =  VhostUserScmi::new(virtio::base_features(ProtectionType::Unprotected), &vuscmi.vhost_user_scmi.socket)
                                .map_err(|_| BackendError::StrError(String::from("vhost scmi new failed")))?;

            vuscmi.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vuscmidev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vuscmi.mmio, vuscmi.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VuScmiDevices {

    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vuscmi_devices,
            VuScmiDevices::create_vuscmi_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vuscmi = VuVirtioScmi::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vuscmi.vhost_user_scmi = VhostUserOption {
                    socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-scmi socket path must be provided"),
                        })?,
                        ),
        };

        if !vuscmi.vhost_user_scmi.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-scmi socket path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-scmi options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-scmi options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vuscmi.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("vhost-user-scmi only supports label"),
                    });
                }
            }
        }

        self.vuscmi_devices.push(vuscmi);
        Ok(())
    }
}

////// VU_VIRTIO_FS //////
struct VuVirtioFs {
    tag: String,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_fs: VhostUserOption,
}

impl VuVirtioFs {
    pub fn new(tag: String) -> Self {
        Self {
            tag,
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_fs: VhostUserOption {
                socket: PathBuf::new()
            },
        }
    }
}

struct VuVirtioFsDevices {
    vufs_devices: Vec<VuVirtioFs>,
}

impl VuVirtioFsDevices {

    pub fn new() -> Self {
        VuVirtioFsDevices { vufs_devices: Vec::new() }
    }

    fn create_vufs_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vfs in &mut self.vufs_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd: &SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vufsdev = VhostUserfs::new(virtio::base_features(ProtectionType::Unprotected), &vfs.vhost_user_fs.socket, vfs.tag.as_str())
                .map_err(|_| BackendError::StrError(String::from("vhost user fs new failed")))?;

            vfs.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vufsdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vfs.mmio, vfs.label, sfd, cfg)?;
        }
        Ok(())
    }
}


impl DeviceTrait for VuVirtioFsDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vufs_devices,
            VuVirtioFsDevices::create_vufs_devices,
            init_config_space
        );
        Ok(handles)
    }
    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vfs = VuVirtioFs::new(String::new());

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vfs.vhost_user_fs = VhostUserOption {
            socket: PathBuf::from(
                components.next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-fs socket path be provided"),
                        })?,
                ),
        };

        if !vfs.vhost_user_fs.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-fs socket path must an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2,'=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-fs options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-fs options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vfs.label = label;
                }
                "tag" => {
                    let tag = value.to_owned();
                    vfs.tag = tag;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("vhost-user-fs only supports label"),
                    });
                }
            }
        }

        self.vufs_devices.push(vfs);
        Ok(())
    }
}


////// VU_VIRTIO_I2C //////
pub struct VuVirtioI2c {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_i2c: VhostUserOption,
}

impl VuVirtioI2c {
    pub fn new() -> Self {
        Self {
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_i2c: VhostUserOption {
                socket: PathBuf::new()
            },
        }
    }
}

struct VuVirtioI2cDevices {
    vui2c_devices: Vec<VuVirtioI2c>,
}

impl VuVirtioI2cDevices {

    pub fn new() -> Self {
        VuVirtioI2cDevices { vui2c_devices: Vec::new() }
    }

    fn create_vui2c_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vi2c in &mut self.vui2c_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd: &SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vui2cdev = VhostUserI2cAdapter::new(virtio::base_features(ProtectionType::Unprotected), &vi2c.vhost_user_i2c.socket)
                .map_err(|_| BackendError::StrError(String::from("vhost user i2c new failed")))?;

            vi2c.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vui2cdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vi2c.mmio, vi2c.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VuVirtioI2cDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vui2c_devices,
            VuVirtioI2cDevices::create_vui2c_devices,
            init_config_space
        );
        Ok(handles)
    }
    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vi2c = VuVirtioI2c::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vi2c.vhost_user_i2c = VhostUserOption {
            socket: PathBuf::from(
                components.next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-i2c socket path be provided"),
                        })?,
                ),
        };

        let mut retries = 0;
        let max_retries = RETRY_LIMIT;
        loop {
            if vi2c.vhost_user_i2c.socket.exists() {
                break;
            }
            retries += 1;
            if retries >= max_retries {
                return Err(argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("vhost-user-i2c socket path must an existing path"),
                });
            }
            sleep(Duration::from_millis(RETRY_DELAY_MS));
        }

        for opt in components {
            let mut o = opt.splitn(2,'=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-i2c options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-i2c options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vi2c.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("vhost-user-i2c only supports label"),
                    });
                }
            }
        }

        self.vui2c_devices.push(vi2c);
        Ok(())
    }
}

////// VU_VIRTIO_GPIO //////
struct VuVirtioGpio {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_gpio: VhostUserOption,
}

struct VuVirtioGpioDevices {
    vugpio_devices: Vec<VuVirtioGpio>,
}

impl VuVirtioGpioDevices {
    pub fn new() -> Self {
        VuVirtioGpioDevices {
            vugpio_devices: Vec::new()
        }
    }

    pub fn create_vugpio_devices(&mut self, cfg: &mut BackendConfig) -> std::result::Result<(), BackendError> {
        for vgpio in &mut self.vugpio_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd: &SafeDescriptor;
            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vugpiodev = VhostUserGpio::new(virtio::base_features(ProtectionType::Unprotected), &vgpio.vhost_user_gpio.socket)
                    .map_err(|_| BackendError::StrError(String::from("vhost user gpio new failed")))?;
            vgpio.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vugpiodev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vgpio.mmio, vgpio.label, sfd, cfg)?;
        }

        Ok(())
    }
}

impl DeviceTrait for VuVirtioGpioDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vugpio_devices,
            VuVirtioGpioDevices::create_vugpio_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {
        let mut vgpio = VuVirtioGpio{label: 0,mmio: None,config_space: Some(Vec::new()),vhost_user_gpio: VhostUserOption {
            socket: PathBuf::new()}};
        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');
        let vu = VhostUserOption {
            socket: PathBuf::from(
                components.next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("missing vhost user gpio sock path"),
                        })?,
                ),
        };
        vgpio.vhost_user_gpio = vu;

        let mut retries = 0;
        let max_retries = RETRY_LIMIT;
        loop {
            if vgpio.vhost_user_gpio.socket.exists() {
                break;
            }
            retries += 1;
            if retries >= max_retries {
                return Err(argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("vhost-user-gpio socket path must an existing path"),
                });
            }
            sleep(Duration::from_millis(RETRY_DELAY_MS));
        }

        for opt in components {
            let mut o = opt.splitn(2,'=');
                let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("vhost-user-gpio options must not be empty"),
                })?;

                let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("vhost-user-gpio options must be of the form `kind=value`"),
                })?;

                match kind {
                    "label" => {
                        let label: u32 = u32::from_str_radix(value, 16)
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be an unsigned integer"),
                            })?;
                        if label == 0 {
                            return Err(argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be a non zero integer"),
                            });

                        }
                        vgpio.label = label;
                    }

                    _ => {
                        return Err(argument::Error::InvalidValue {
                            value: kind.to_owned(),
                            expected: String::from("vhost-user-gpio only supports label"),
                        });
                    }
                }
            }
        self.vugpio_devices.push(vgpio);
        Ok(())
    }
}

////// VU_VIRTIO_FRPC //////
struct VuVirtiofrpc {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_frpc: VhostUserOption,
}

impl VuVirtiofrpc {
    pub fn new() -> Self {
        VuVirtiofrpc {
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_frpc: VhostUserOption{
                socket: PathBuf::new()
            },
        }
    }
}

struct VuVirtiofrpcDevices {
    vu_frpc_devices: Vec<VuVirtiofrpc>,
}
impl VuVirtiofrpcDevices {
    pub fn new() -> Self {
        VuVirtiofrpcDevices { vu_frpc_devices: Vec::new() }
    }
    fn create_vufrpc_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vufrpc in &mut self.vu_frpc_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vufrpcdev =  VhostUserfrpc::new(virtio::base_features(ProtectionType::Unprotected), &vufrpc.vhost_user_frpc.socket)
                                .map_err(|_| BackendError::StrError(String::from("vhost frpc new failed")))?;

            vufrpc.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vufrpcdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vufrpc.mmio, vufrpc.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VuVirtiofrpcDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vu_frpc_devices,
            VuVirtiofrpcDevices::create_vufrpc_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vufrpc = VuVirtiofrpc::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vufrpc.vhost_user_frpc = VhostUserOption {
                    socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-frpc socket path must be provided"),
                        })?,
                        ),
        };

        if !vufrpc.vhost_user_frpc.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-frpc socket path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-frpc options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-frpc options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    vufrpc.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("vhost-user-frpc only supports label"),
                    });
                }
            }
        }

        self.vu_frpc_devices.push(vufrpc);
        Ok(())
    }
}

////// VU_VIRTIO_SSR //////
struct VuVirtioSsr {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_ssr: VhostUserOption,
}

struct VuVirtioSsrDevices {
    vussr_devices: Vec<VuVirtioSsr>,
}

impl VuVirtioSsrDevices {
    pub fn new() -> Self {
        VuVirtioSsrDevices {
            vussr_devices: Vec::new()
        }
    }

    pub fn create_vussr_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {
        for vssr in &mut self.vussr_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd: &SafeDescriptor;
            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let vussrdev = VhostUserSsr::new(virtio::base_features(ProtectionType::Unprotected), &vssr.vhost_user_ssr.socket)
                    .map_err(|_| BackendError::StrError(String::from("vhost user ssr new failed")))?;
            vssr.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vussrdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vssr.mmio, vssr.label, sfd, cfg)?;
        }

        Ok(())
    }
}

impl DeviceTrait for VuVirtioSsrDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vussr_devices,
            VuVirtioSsrDevices::create_vussr_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {
        let mut vssr = VuVirtioSsr{label: 0,mmio: None,config_space: Some(Vec::new()),vhost_user_ssr: VhostUserOption {
                                                    socket: PathBuf::new()}};
        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');
        let vu = VhostUserOption {
            socket: PathBuf::from(
                components.next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("missing vhost user ssr sock path"),
                        })?,
                ),
        };
        vssr.vhost_user_ssr = vu;

        let mut retries = 0;
        let max_retries = RETRY_LIMIT;
        loop {
            if vssr.vhost_user_ssr.socket.exists() {
                break;
            }
            retries += 1;
            if retries >= max_retries {
                return Err(argument::Error::InvalidValue {
                    value: param.to_owned(),
                    expected: String::from("vhost-user-ssr socket path must an existing path"),
                });
            }
            sleep(Duration::from_millis(RETRY_DELAY_MS));
        }
        for opt in components {
            let mut o = opt.splitn(2,'=');
                let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("vhost-user-ssr options must not be empty"),
                })?;

                let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                    value: opt.to_owned(),
                    expected: String::from("vhost-user-ssr options must be of the form `kind=value`"),
                })?;

                match kind {
                    "label" => {
                        let label: u32 = u32::from_str_radix(value, 16)
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be an unsigned integer"),
                            })?;
                        if label == 0 {
                            return Err(argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be a non zero integer"),
                            });

                        }
                        vssr.label = label;
                    }

                    _ => {
                        return Err(argument::Error::InvalidValue {
                            value: kind.to_owned(),
                            expected: String::from("vhost-user-ssr only supports label"),
                        });
                    }
                }
            }
        self.vussr_devices.push(vssr);
        Ok(())
    }
}

////// VU_VIRTIO_GENERIC //////
#[cfg(feature = "vhost-user-generic")]
struct VuVirtioGeneric {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    num_queues: Option<u64>,
    vhost_user_generic: VhostUserOption,
}

#[cfg(feature = "vhost-user-generic")]
impl VuVirtioGeneric {
    pub fn new() -> Self {
        VuVirtioGeneric{
            label: 0,
            num_queues: None,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_generic: VhostUserOption {
                socket: PathBuf::new(),
            },
        }
    }
}

#[cfg(feature = "vhost-user-generic")]
struct VuVirtioGenericDevices {
    vugeneric_devices: Vec<VuVirtioGeneric>,
}

#[cfg(feature = "vhost-user-generic")]
impl VuVirtioGenericDevices {
    pub fn new() -> Self {
        VuVirtioGenericDevices {
            vugeneric_devices: Vec::new()
        }
    }

    pub fn create_vugeneric_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for vgen in &mut self.vugeneric_devices {

            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd: &SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };

            let vu_generic_dev = VhostUserGeneric::new(virtio::base_features(ProtectionType::Unprotected), &vgen.vhost_user_generic.socket, vgen.num_queues)
                    .map_err(|_| BackendError::StrError(String::from("vhost user generic new failed")))?;

            vgen.mmio = Some(MmioDevice::new(mem.clone(), Box::new(vu_generic_dev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vgen.mmio, vgen.label, sfd, cfg)?;
        }

        Ok(())
    }
}

#[cfg(feature = "vhost-user-generic")]
impl DeviceTrait for VuVirtioGenericDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.vugeneric_devices,
            VuVirtioGenericDevices::create_vugeneric_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {
        let mut vgen = VuVirtioGeneric::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        vgen.vhost_user_generic = VhostUserOption {
            socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-generic socket path must be provided"),
                        })?,
                        ),
        };

        if !vgen.vhost_user_generic.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-generic socket path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-generic options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-generic options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                            .map_err(|_| argument::Error::InvalidValue {
                                value: value.to_owned(),
                                expected: String::from("`label` must be an unsigned integer"),
                                })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                            });
                        }
                    vgen.label = label;
                }
                "queue-num" => {
                    let num_of_queues: u64;
                    num_of_queues = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("queue number must be an unsigned integer "),
                    })?;
                    vgen.num_queues = Some(num_of_queues)
                }

                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported vhost-user-generic options only"),
                    });
                }
            }
        }

        self.vugeneric_devices.push(vgen);
        Ok(())

    }
}

////// VCPU //////
struct Vcpu {
    id: u8,
    raw_fd: i32,
    thread_handle: Option<JoinHandle<()>>,
}

impl Vcpu {
    fn run_vcpu(&mut self, vm_name: &str) -> Result<JoinHandle<()>, BackendError>{
        let builder = thread::Builder::new()
        .name(format!("{}_vcpu{}", vm_name, self.id));
        let vm = vm_name.to_string();
        let id = self.id;
        let raw_fd = self.raw_fd;
        builder.spawn(move || {
            loop {
                let ret = unsafe { libc::ioctl(raw_fd, GH_VCPU_RUN()) };
                if ret == 0 {
                    error!("{}", format!("{}_vcpu{} returned 0", vm, id));
                    std::process::exit(0);
                }
                else {
                    error!("{}", format!("{}_vcpu{} exited with reason {}", vm, id, ret));
                    panic!("{}", format!("{}_vcpu{} exited with reason {}", vm, id, ret));
                }
            }
        }).map_err(|_| BackendError::StrNumError {
            err: format!("{}_vcpu{} thread create failed", vm_name, self.id),
            val: io::Error::last_os_error(),
        })
    }
}

struct Vcpus {
    vcpus: Vec<Vcpu>,
    vcpu_count: u16,
}

impl Vcpus {
    pub fn new() -> Self {
        Vcpus {vcpus: Vec::new(), vcpu_count: 1}
    }
    fn create_vcpus(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {
        let vm_sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));
        for vcpu_id in 0..self.vcpu_count{
            let vcpu_fd = unsafe { libc::ioctl(vm_sfd.as_raw_descriptor(), GH_CREATE_VCPU(), vcpu_id as c_uint) };
            if vcpu_fd < 0 {
                return Err(BackendError::StrNumError {
                    err: String::from("create vcpu ioctl failed"),
                    val: io::Error::last_os_error(),});
            }
            self.vcpus.push(Vcpu {id: vcpu_id as u8, raw_fd: vcpu_fd, thread_handle: None});
        }
        Ok(())
    }
    fn run_vcpus(&mut self, cfg: &mut BackendConfig) ->  Result<Vec<JoinHandle<()>>, BackendError> {
        let mut handles = vec![];
        for vcpu in &mut self.vcpus {
            let vm_name = cfg.vm.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let handle = vcpu.run_vcpu(vm_name);
            if let Err(_handle) = handle {
                return Err(_handle);
            }
            handles.push(handle.expect(&format!("{}:{}", file!(), line!())));
        }
        Ok(handles)
    }
}

impl DeviceTrait for Vcpus {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        self.create_vcpus(cfg);
        let handles = self.run_vcpus(cfg).expect(&format!("{}:{}", file!(), line!()));
        Ok(handles)
    }
    fn set_argument(&mut self, _value: Option<&str>) -> argument::Result<()> {
        // Dummy implementation as we are not taking cmd-line input for vcpu.
        Ok(())
    }
}

///// SCMI DEVICE /////
struct ScmiDevice {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}
impl ScmiDevice {
    pub fn new() -> Self {
        ScmiDevice {
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
        }
    }

    pub fn create_vhost_scmi_device(&self, _mem: &GuestMemory) -> Result<Box<Scmi>, BackendError> {
        let features :u64 = base_features(ProtectionType::Unprotected);
        let vhost_scmi_dev_path = PathBuf::from(VHOST_SCMI_PATH);
        let dev = virtio::vhost::Scmi::new(&vhost_scmi_dev_path, features)
            .map_err(|_| BackendError::StrError(String::from("virtio scmi new failed")))?;

        Ok(Box::new(dev))
    }
}

struct ScmiDevices {
    scmi_devices: Vec<ScmiDevice>,
}

impl ScmiDevices {
    pub fn new() -> Self {
        ScmiDevices {scmi_devices: Vec::new() }
    }

    fn create_scmi_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for scmi in &mut self.scmi_devices {
            let mem = cfg.mem.as_ref().unwrap();
            let scmidev = scmi.create_vhost_scmi_device(mem)?;
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };

            scmi.mmio = Some(MmioDevice::new(mem.clone(), scmidev).unwrap());
            mmio_handle(&scmi.mmio, scmi.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for ScmiDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.scmi_devices,
            ScmiDevices::create_scmi_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut scmi = ScmiDevice::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        let _next = components.next();

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("scmi options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("scmi options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    scmi.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported scmi options only"),
                    });
                }
            }
        }

        self.scmi_devices.push(scmi);
        Ok(())
    }
}

///// VSOCK DEVICE /////
struct VirtioSock {
    context_id: u64,
    vhost_vsock_path: PathBuf,
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
}

impl VirtioSock {
    fn new() -> Self {
        VirtioSock {
            context_id: 0,
            vhost_vsock_path: PathBuf::from(VSOCK_PATH),
            label: 0,
            mmio: None,
            config_space: Some(Vec::new())
        }
    }
}
struct VirtioSockDevices {
    v_sock_devices: Vec<VirtioSock>,
}

impl VirtioSockDevices {
    pub fn new() -> Self {
        VirtioSockDevices { v_sock_devices: Vec::new(), }
    }

    fn create_vsock_devices(&mut self, cfg: &mut BackendConfig) -> std::result::Result<(), BackendError> {

        for vsock in &mut self.v_sock_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };

            let v_sock_device = Vsock::new(
                virtio::base_features(ProtectionType::Unprotected),
                &VhostVsockConfig{
                    device: virtio::vhost::vsock::VhostVsockDeviceParameter::Path(vsock.vhost_vsock_path.clone()),
                    cid: vsock.context_id
                }
            );

            vsock.mmio = Some(MmioDevice::new(mem.clone(), Box::new(v_sock_device.expect(&format!("{}:{}", file!(), line!())))).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&vsock.mmio, vsock.label, sfd, cfg);
        }
        Ok(())
    }
}
impl DeviceTrait for VirtioSockDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.v_sock_devices,
            VirtioSockDevices::create_vsock_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut vsock = VirtioSock::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vsock cid must be present"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vsock cid must be of the form `cid=value`")
            })?;

            match kind {
                "cid" => {
                    vsock.context_id = value.parse().map_err(|_| argument::Error::InvalidValue {
                        value: value.to_owned(),
                        expected: String::from("context id must be an integer")
                    })?;
                }
                "label" => {
                    let label = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label must be an unsigned integer`")
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non-zero integer"),
                        });
                    }
                    vsock.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("supported vsock options are cid and label"),
                    });
                }
            }
        }

        self.v_sock_devices.push(vsock);
        Ok(())
    }
}

////// VIRTIO EAVB DEVICE //////
struct VirtioEAVB {
    label: u32,
    mmio: Option<MmioDevice>,
    config_space: Option<Vec<u32>>,
    vhost_user_eavb: VhostUserOption,
}

impl VirtioEAVB {
    pub fn new() -> Self {
        VirtioEAVB {
            label: 0,
            mmio: None,
            config_space: Some(Vec::new()),
            vhost_user_eavb: VhostUserOption{
                socket: PathBuf::new()
            },
        }
    }
}

struct VirtioEAVBDevices {
    virtio_eavb_devices: Vec<VirtioEAVB>,
}
impl VirtioEAVBDevices {
    pub fn new() -> Self {
        VirtioEAVBDevices { virtio_eavb_devices: Vec::new() }
    }
    fn create_veavb_devices(&mut self, cfg: &mut BackendConfig) -> Result<(), BackendError> {

        for veavb in &mut self.virtio_eavb_devices {
            let mem = cfg.mem.as_ref().expect(&format!("{}:{}", file!(), line!()));
            let sfd :&SafeDescriptor;

            match cfg.driver_variant {
                1 => {sfd = cfg.sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                2 => {sfd = cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()))}
                _ => return Err(BackendError::StrError(String::from("Unsupported driver variant.")))
            };
            let eavbdev =  VhostUserEAVB::new(virtio::base_features(ProtectionType::Unprotected), &veavb.vhost_user_eavb.socket)
                                .map_err(|_| BackendError::StrError(String::from("vhost eavb new failed")))?;

            veavb.mmio = Some(MmioDevice::new(mem.clone(), Box::new(eavbdev)).expect(&format!("{}:{}", file!(), line!())));
            mmio_handle(&veavb.mmio, veavb.label, sfd, cfg)?;
        }
        Ok(())
    }
}

impl DeviceTrait for VirtioEAVBDevices {
    fn create_and_run_devices(&mut self, cfg: &mut BackendConfig) -> Result<Vec<JoinHandle<()>>, ()> {
        let handles = create_device_threads!(
            self,
            cfg,
            &mut self.virtio_eavb_devices,
            VirtioEAVBDevices::create_veavb_devices,
            init_config_space
        );
        Ok(handles)
    }

    fn set_argument(&mut self, value: Option<&str>) -> argument::Result<()> {

        let mut veavb = VirtioEAVB::new();

        let param = value.expect(&format!("{}:{}", file!(), line!()));
        let mut components = param.split(',');

        veavb.vhost_user_eavb = VhostUserOption {
                    socket: PathBuf::from(
                        components
                        .next()
                        .ok_or_else(|| argument::Error::InvalidValue {
                            value: param.to_owned(),
                            expected: String::from("vhost-user-eavb socket path must be provided"),
                        })?,
                        ),
        };

        if !veavb.vhost_user_eavb.socket.exists() {
            return Err(argument::Error::InvalidValue {
                value: param.to_owned(),
                expected: String::from("vhost-user-eavb socket path must be an existing path"),
            });
        }

        for opt in components {
            let mut o = opt.splitn(2, '=');
            let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-eavb options must not be empty"),
            })?;

            let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                value: opt.to_owned(),
                expected: String::from("vhost-user-eavb options must be of the form `kind=value`"),
            })?;

            match kind {
                "label" => {
                    let label: u32 = u32::from_str_radix(value, 16)
                        .map_err(|_| argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be an unsigned integer"),
                        })?;
                    if label == 0 {
                        return Err(argument::Error::InvalidValue {
                            value: value.to_owned(),
                            expected: String::from("`label` must be a non zero integer"),
                        });
                    }
                    veavb.label = label;
                }
                _ => {
                    return Err(argument::Error::InvalidValue {
                        value: kind.to_owned(),
                        expected: String::from("vhost-user-eavb only supports label"),
                    });
                }
            }
        }

        self.virtio_eavb_devices.push(veavb);
        Ok(())
    }
}

/// Aggregate of all configurable options for a block device
struct BackendConfig {
    sfd: Option<SafeDescriptor>,
    vm_sfd: Option<SafeDescriptor>,
    vm: Option<String>,
    mem: Option<GuestMemory>,
    driver_variant: u8,
    sandbox: bool,
    non_protected_virtio: bool,
    log_level: LevelFilter,
    log_type: Option<String>,
    bkend_dev_exist: bool,
}

impl Default for BackendConfig {
    fn default() -> BackendConfig {
        BackendConfig {
            vm: None,
            mem: None,
            sfd: None,
            vm_sfd: None,
            driver_variant: 2,
            sandbox: false,
            non_protected_virtio: false,
            log_level: log::LevelFilter::Info,
            log_type: Some("ftrace".to_string()),
            bkend_dev_exist: false,
        }
    }
}

impl BackendConfig {
    pub fn new() -> Self {
        BackendConfig::default()
    }
}

struct VMBackend {
    cfg: BackendConfig,
    vdisk_devices: VirtioDiskDevices,
    vnet_devices: VirtioNetDevices,
    vugp_devices: VuGPDevices,
    vcpus: Vcpus,
    scmi_devices: ScmiDevices,
    vuscmi_devices: VuScmiDevices,
    vuvirtio_i2c_devices: VuVirtioI2cDevices,
    vuvirtio_fs_devices: VuVirtioFsDevices,
    vuvirtio_frpc_devices: VuVirtiofrpcDevices,
    vhab_devices: VirtioHabDevices,
    vinput_devices: VirtioInputDevices,
    vconsole_devices: VirtioConsoleDevices,
    vuvirtio_ssr_devices: VuVirtioSsrDevices,
    vuvirtio_gpio_devices: VuVirtioGpioDevices,
    vsock_devices: VirtioSockDevices,
    veavb_devices: VirtioEAVBDevices,
    #[cfg(feature = "vhost-user-generic")]
    vugeneric_devices: VuVirtioGenericDevices,
}

impl VMBackend {
    pub fn new() -> Self {
        VMBackend {
            cfg: BackendConfig::new(),
            vdisk_devices: VirtioDiskDevices::new(),
            vnet_devices: VirtioNetDevices::new(),
            vugp_devices: VuGPDevices::new(),
            vcpus: Vcpus::new(),
            scmi_devices: ScmiDevices::new(),
            vuscmi_devices: VuScmiDevices::new(),
            vuvirtio_i2c_devices: VuVirtioI2cDevices::new(),
            vuvirtio_fs_devices: VuVirtioFsDevices::new(),
            vuvirtio_frpc_devices: VuVirtiofrpcDevices::new(),
            vhab_devices: VirtioHabDevices::new(),
            vinput_devices: VirtioInputDevices::new(),
            vconsole_devices: VirtioConsoleDevices::new(),
            vuvirtio_ssr_devices: VuVirtioSsrDevices::new(),
            vuvirtio_gpio_devices: VuVirtioGpioDevices::new(),
            vsock_devices: VirtioSockDevices::new(),
            veavb_devices: VirtioEAVBDevices::new(),
            #[cfg(feature = "vhost-user-generic")]
            vugeneric_devices: VuVirtioGenericDevices::new(),
        }
    }

    fn set_argument(&mut self, name: &str, value: Option<&str>) -> argument::Result<()> {
        match name {
            "disk" => {
                self.cfg.bkend_dev_exist = true;
                self.vdisk_devices.set_argument(value)?
            }
            "vm" => {
                self.cfg.vm = Some(value.expect(&format!("{}:{}", file!(), line!())).to_owned());
                //PID would be required for log analysis of all log levels. Hence error!().
                error!("{}", format!("qcrosvm PID for {}: {}", self.cfg.vm.as_ref()
                                        .expect(&format!("{}:{}", file!(), line!())), process::id()));
            }
            "sandbox" => {
                self.cfg.sandbox = true;
            }
            "use-non-protected-virtio" => {
                self.cfg.non_protected_virtio = true;
            }
            "log" => {
                let param = value.expect(&format!("{}:{}", file!(), line!()));
                let components = param.split(',');
                for opt in components {
                    let mut o = opt.splitn(2, '=');
                    let kind = o.next().ok_or_else(|| argument::Error::InvalidValue {
                        value: opt.to_owned(),
                        expected: String::from("log options must not be empty"),
                    })?;
                    let value = o.next().ok_or_else(|| argument::Error::InvalidValue {
                        value: opt.to_owned(),
                        expected: String::from("log options must be of the form `kind=value`"),
                    })?;
                    match kind {
                        "level" => {
                            let level = value.to_owned();
                            match Level::from_str(&level)
                            {
                                Ok(temp_log_level) => {
                                    // Reset the logging level
                                    self.cfg.log_level = temp_log_level.to_level_filter();
                                }
                                Err(_) =>  {
                                    return Err(argument::Error::InvalidValue {
                                        value: level,
                                        expected: String::from("trace | debug | info | warn | error"),
                                    });
                                }
                            }
                        }
                        "type" => {
                            let logger_type = value.to_owned();
                            match logger_type.as_str() {
                                "logcat"|"term"|"ftrace" => {
                                    self.cfg.log_type = Some(logger_type);
                                }
                                _ => {
                                    return Err(argument::Error::InvalidValue {
                                        value: value.to_owned(),
                                        expected: String::from
                                            ("supported logger options. 'type=logcat|term|ftrace"),
                                    });
                                }
                            }
                        }
                        _ => {
                            return Err(argument::Error::InvalidValue {
                                value: kind.to_owned(),
                                expected: String::from("supported logger options. 'type=logcat | term | ftrace'"),
                            });
                        }
                    }
                }
            }
            "net" => {
                self.cfg.bkend_dev_exist = true;
                self.vnet_devices.set_argument(value)?
            }
            "vhost-user-gp" => {
                self.cfg.bkend_dev_exist = true;
                self.vugp_devices.set_argument(value)?
            }
            "scmi" => {
                self.cfg.bkend_dev_exist = true;
                self.scmi_devices.set_argument(value)?
            }
            "vhost-user-scmi" => {
                self.cfg.bkend_dev_exist = true;
                self.vuscmi_devices.set_argument(value)?
            }
            "vhost-user-i2c" => {
                self.cfg.bkend_dev_exist = true;
                self.vuvirtio_i2c_devices.set_argument(value)?
            }
            "vhost-user-fs" => {
                self.cfg.bkend_dev_exist = true;
                self.vuvirtio_fs_devices.set_argument(value)?
            }
            "vhost-user-frpc" => {
                self.cfg.bkend_dev_exist = true;
                self.vuvirtio_frpc_devices.set_argument(value)?
            }
            "vhost-user-ssr" => {
                self.cfg.bkend_dev_exist = true;
                self.vuvirtio_ssr_devices.set_argument(value)?
            }
            "vhost-user-gpio" => {
                self.cfg.bkend_dev_exist = true;
                self.vuvirtio_gpio_devices.set_argument(value)?
            }
            "vhost-user-hab" => {
                self.cfg.bkend_dev_exist = true;
                self.vhab_devices.set_argument(value)?
            }
            "input" => {
                self.cfg.bkend_dev_exist = true;
                self.vinput_devices.set_argument(value)?
            }
            "console" => {
                self.cfg.bkend_dev_exist = true;
                self.vconsole_devices.set_argument(value)?
            }
            "vsock" => {
                self.cfg.bkend_dev_exist = true;
                self.vsock_devices.set_argument(value)?
            }
            "vhost-user-eavb" => {
                self.cfg.bkend_dev_exist = true;
                self.veavb_devices.set_argument(value)?
            }
            #[cfg(feature = "vhost-user-generic")]
            "vhost-user-generic" => {
                self.cfg.bkend_dev_exist = true;
                self.vugeneric_devices.set_argument(value)?
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    fn set_logger(&mut self) -> Result<(), ()>
    {
        let mut log_tag = String::from(LOG_TAG);

        let cfg = &self.cfg;
        if !cfg.vm.is_none() {
            log_tag.push('_');
            log_tag.push_str(cfg.vm.as_ref().expect(&format!("{}:{}", file!(), line!())));
        }
        match cfg.log_type.as_ref().expect(&format!("{}:{}", file!(), line!())).as_str() {
            "logcat" => {
                android_logger::init_once(
                    Config::default()
                    .with_min_level(Level::Trace)
                    .with_tag(log_tag.as_str()));
                log::set_max_level(cfg.log_level);
            }
            "term" => {
                let config = ConfigBuilder::new()
                    .set_time_level(LevelFilter::Off)
                    .set_max_level(LevelFilter::Off)
                    .set_location_level(LevelFilter::Off)
                    .set_thread_level(LevelFilter::Off)
                    .set_target_level(LevelFilter::Off)
                    .with_tag(log_tag.as_str())
                    .build();
                let _init = SimpleLogger::init(cfg.log_level, config);
            }
            //Default logger
            "ftrace" => {
                let config = ConfigBuilder::new()
                    .set_time_level(LevelFilter::Off)
                    .set_max_level(LevelFilter::Off)
                    .set_location_level(LevelFilter::Off)
                    .set_thread_level(LevelFilter::Off)
                    .set_target_level(LevelFilter::Off)
                    .with_tag(log_tag.as_str())
                    .without_new_line()
                    .build();
                let _init = WriteLogger::init(cfg.log_level, config, File::create(TRACE_MARKER)
                                            .expect(&format!("{}:{}", file!(), line!())));
            }
            _ => {}
        }
        return Ok(())
    }

    fn run_backend_v2(&mut self) -> Result<(), ()>
    {
        let file_name = format!("{}", GH_PATH);
        let fd: i32 = unsafe { open(file_name.as_ptr() as *const c_char, O_RDWR) };
        if fd < 0 {
            error!("{}", format!("Error: device node open failed {:?}", io::Error::last_os_error()));
            panic!("{}", format!("Error: device node open failed {:?}", io::Error::last_os_error()));
        }
        self.cfg.sfd = Some(unsafe { SafeDescriptor::from_raw_descriptor(fd) });

        self.cfg.driver_variant = 2;

        let sfd = self.cfg.sfd.as_mut().expect(&format!("{}:{}", file!(), line!())).try_clone()
            .expect(&format!("{}:{}", file!(), line!()));
        let vm_fd = unsafe { libc::ioctl(sfd.as_raw_descriptor(), GH_CREATE_VM()) };
        if vm_fd < 0 {
            error!("{}", format!("Error: create vm ioctl failed with error {:?}", io::Error::last_os_error()));
            panic!("{}", format!("Error: create vm ioctl failed with error {:?}", io::Error::last_os_error()));
        }

        self.cfg.vm_sfd = Some(unsafe { SafeDescriptor::from_raw_descriptor(vm_fd) });
        let vm_sfd = self.cfg.vm_sfd.as_ref().expect(&format!("{}:{}", file!(), line!()));

        let vm_name = self.cfg.vm.as_ref().expect(&format!("{}:{}", file!(), line!()));
        let mut fw_name = fw_name {_name: [0; 16],};
        fw_name._name[..vm_name.len()].copy_from_slice(vm_name.as_bytes());
        let ret = unsafe { ioctl_with_ref(vm_sfd, GH_VM_SET_FW_NAME(), &fw_name) };
        if ret != 0 {
            error!("{}", format!("Error: set fw name ioctl failed with error {:?}", io::Error::last_os_error()));
            panic!("{}", format!("Error: set fw name ioctl failed with error {:?}", io::Error::last_os_error()));
        }

        // CPU Count
        let vcpu_count = unsafe { libc::ioctl(vm_fd, GH_GET_VCPU_COUNT()) };
        if vcpu_count < 0 || vcpu_count > (GH_VCPU_MAX).try_into().expect(&format!("{}:{}", file!(), line!())) {
            error!("{}", format!("Error: get vcpu count ioctl failed {:?}", io::Error::last_os_error()));
            panic!("{}", format!("Error: get vcpu count ioctl failed {:?}", io::Error::last_os_error()));
        }
        self.vcpus.vcpu_count = vcpu_count as u16;
        info!("{}", format!("vcpu_count {}", self.vcpus.vcpu_count));

        if self.cfg.non_protected_virtio {
            let vm_mem_count = unsafe { libc::ioctl(vm_fd, GH_VM_GET_MEM_COUNT()) };
            if vm_mem_count <= 0 {
                error!("{}", format!("Error: get vm mem count ioctl failed {:?}", io::Error::last_os_error()));
                panic!("{}", format!("Error: get vm mem count ioctl failed {:?}", io::Error::last_os_error()));
            }
            info!("{}", format!("vm_mem_count {}", vm_mem_count));

            let mut mem_ranges = Vec::new();
            for mem_idx in 0..vm_mem_count as u8 {
                let mut mem_region = VmMemRegion {
                    _mem_idx: mem_idx,
                    _mem_phys: 0,
                    _mem_size: 0,
                    _fd: 0,
                };

                let ret = unsafe { ioctl_with_mut_ref(vm_sfd, GH_VM_GET_MEM_REGION(), &mut mem_region) };
                if ret != 0 {
                    error!("{}", format!("Error: get vm mem region ioctl failed {:?}", io::Error::last_os_error()));
                    panic!("{}", format!("Error: get vm mem region ioctl failed {:?}", io::Error::last_os_error()));
                }

                let _ = create_vm_mem_region((GuestAddress(mem_region._mem_phys), mem_region._mem_size), &mem_region._fd, &mut mem_ranges);
            }

            if self.cfg.bkend_dev_exist {
                self.cfg.mem = Some(GuestMemory::from_regions(mem_ranges)
                .expect(&format!("{}:{}", file!(), line!())));
            }

        } else {
            if self.cfg.bkend_dev_exist {
                let mut shmem_size: u64 = 0;
                let ret = unsafe { ioctl_with_mut_ref(vm_sfd, GET_SHARED_MEMORY_SIZE_V2(), &mut shmem_size) };
                if ret != 0 || shmem_size == 0 {
                    error!("{}", format!("Error: get vm shared memory size ioctl failed {:?}", io::Error::last_os_error()));
                    panic!("{}", format!("Error: get vm shared memory size ioctl failed {:?}", io::Error::last_os_error()));
                }
                info!("{}", format!("shmem_size {}", shmem_size));

                self.cfg.mem = Some(self::new_from_rawfd(&[(GuestAddress(0), shmem_size)], &vm_fd)
                                .expect(&format!("{}:{}", file!(), line!())));
            }
        }

        // collect all the devices
        let mut devices: Vec<&mut dyn DeviceTrait> = vec![
            &mut self.vdisk_devices,
            &mut self.vnet_devices,
            &mut self.vhab_devices,
            &mut self.vinput_devices,
            &mut self.scmi_devices,
            &mut self.vconsole_devices,
            &mut self.vuscmi_devices,
            &mut self.vuvirtio_i2c_devices,
            &mut self.vuvirtio_fs_devices,
            &mut self.vugp_devices,
            &mut self.vuvirtio_frpc_devices,
            &mut self.vuvirtio_ssr_devices,
            &mut self.vuvirtio_gpio_devices,
            &mut self.vsock_devices,
            #[cfg(feature = "vhost-user-generic")]
            &mut self.vugeneric_devices,
            &mut self.vcpus,    // vCPU create and run at the end
            &mut self.veavb_devices,
        ];

        let mut device_thread_vecs = vec![];

        // create and run all the devices
        for device in devices {
            device_thread_vecs.push(device.create_and_run_devices(&mut self.cfg)?);
        }

        // wait on all the threads, starting with the vCPUs
        for device_threads in device_thread_vecs {
            for thread in device_threads {
                let _ = thread.join();
            }
        }

        Ok(())
    }
    fn run_backend(&mut self) -> Result<(), ()> {
        if self.cfg.vm.is_none() {
            error!("Error: missing vm argument");
            print_usage();
            panic!("Error: missing vm argument");
        }

        // Enforce the current process to be jailed.
        if self.cfg.sandbox {
            match set_minijail(CROSVM_MINIJAIL_POLICY){
                Ok(_) => {
                    debug!("Sandboxing using minijail is enabled!!");
                }
                Err(_) => {
                    error!("Minijail enforcement failed!!");
                    panic!("Minijail enforcement failed!!");
                }
            }
        }

        return self.run_backend_v2()
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct fw_name {
    _name: [::std::os::raw::c_char; 16usize],
}

#[repr(C)]
struct VirtioEventfd {
    _label: u32,
    _flags: u32,
    _queue_num: u32,
    _fd: RawFd,
}

#[repr(C)]
struct VirtioIrqfd {
    _label: u32,
    _flags: u32,
    _fd: RawFd,
    _reserved: u32,
}

#[repr(C)]
struct VirtioEvent {
    _label: u32,
    _event: u32,
    _event_data: u32,
    _reserved: u32,
}

#[repr(C)]
struct VirtioDevFeatures {
    _label: u32,
    _reserved: u32,
    _features_sel: u32,
    _features: u32,
}

#[repr(C)]
struct VirtioQueueMax {
    _label: u32,
    _reserved: u32,
    _queue_sel: u32,
    _queue_num_max: u32,
}

#[repr(C)]
struct VirtioConfigData {
    _label: u32,
    _config_size: u32,
    _config_data: *mut libc::c_char,
}

#[repr(C)]
struct VirtioQueueInfo {
    _label: u32,
    _queue_sel: u32,
    _queue_num: u32,
    _queue_ready: u32,
    _queue_desc: u64,
    _queue_driver: u64,
    _queue_device: u64,
}

#[repr(C)]
struct VirtioDriverFeatures {
    _label: u32,
    _reserved: u32,
    _features_sel: u32,
    _features: u32,
}

#[repr(C)]
struct VirtioAckReset {
    _label: u32,
    _reserved: u32,
}

#[repr(C)]
struct VirtioInputDeviceConfig {
    _label: u32,
    _device_id: u64,
    _prop_bits: u32,
    _num_ev_types: u8,
    _num_abs_axes: u8,
    _reserved: u32,
}

#[repr(C)]
struct VirtioInputDeviceData {
    _label: u32,
    _sel: u8,
    _subsel: u8,
    _size: u8,
    _reserved: [u8; 5],
    _payload: [u8; 128],
}

#[repr(C)]
struct VmMemRegion {
    _mem_idx: u8,
    _mem_phys: u64,
    _mem_size: u64,
    _fd: RawFd,
}

/* system ioctls */
ioctl_io_nr!(GH_CREATE_VM,          GH_IOCTL_TYPE_V2, 0x01);
/* vm ioctls */
ioctl_io_nr!(GH_CREATE_VCPU,            GH_IOCTL_TYPE_V2, 0x40);
ioctl_iow_nr!(GH_VM_SET_FW_NAME,        GH_IOCTL_TYPE_V2, 0x41, fw_name);
ioctl_ior_nr!(GH_VM_GET_FW_NAME,        GH_IOCTL_TYPE_V2, 0x42, fw_name);
ioctl_io_nr!(GH_GET_VCPU_COUNT,         GH_IOCTL_TYPE_V2, 0x43);
ioctl_io_nr!(GH_VM_GET_MEM_COUNT,       GH_IOCTL_TYPE_V2, 0x44);
ioctl_iowr_nr!(GH_VM_GET_MEM_REGION,    GH_IOCTL_TYPE_V2, 0x45, VmMemRegion);

/* vm ioctls for virtio backend driver */
ioctl_ior_nr!(GET_SHARED_MEMORY_SIZE_V2,    GH_IOCTL_TYPE_V2, 0x61, u64);
ioctl_iow_nr!(IOEVENTFD_V2,                 GH_IOCTL_TYPE_V2, 0x62, VirtioEventfd);
ioctl_iow_nr!(IRQFD_V2,                     GH_IOCTL_TYPE_V2, 0x63, VirtioIrqfd);
ioctl_iowr_nr!(WAIT_FOR_EVENT_V2,           GH_IOCTL_TYPE_V2, 0x64, VirtioEvent);
ioctl_iow_nr!(SET_DEVICE_FEATURES_V2,       GH_IOCTL_TYPE_V2, 0x65, VirtioDevFeatures);
ioctl_iow_nr!(SET_QUEUE_NUM_MAX_V2,         GH_IOCTL_TYPE_V2, 0x66, VirtioQueueMax);
ioctl_iow_nr!(SET_DEVICE_CONFIG_DATA_V2,    GH_IOCTL_TYPE_V2, 0x67, VirtioConfigData);
ioctl_iowr_nr!(GET_DRIVER_CONFIG_DATA_V2,   GH_IOCTL_TYPE_V2, 0x68, VirtioConfigData);
ioctl_iowr_nr!(GET_QUEUE_INFO_V2,           GH_IOCTL_TYPE_V2, 0x69, VirtioQueueInfo);
ioctl_iowr_nr!(GET_DRIVER_FEATURES_V2,      GH_IOCTL_TYPE_V2, 0x6a, VirtioDriverFeatures);
ioctl_iowr_nr!(ACK_DRIVER_OK_V2,            GH_IOCTL_TYPE_V2, 0x6b, u32);
ioctl_io_nr!(SET_APP_READY_V2,              GH_IOCTL_TYPE_V2, 0x6c);
ioctl_iow_nr!(ACK_RESET_V2,                 GH_IOCTL_TYPE_V2, 0x6d, VirtioAckReset);
ioctl_iow_nr!(SET_INPUT_DEVICE_CONFIG_V2,   GH_IOCTL_TYPE_V2, 0x6e, VirtioInputDeviceConfig);
ioctl_iow_nr!(SET_INPUT_DEVICE_DATA_V2,     GH_IOCTL_TYPE_V2, 0x6f, VirtioInputDeviceData);

/* virtio backend driver ioctls for backward compatibility */
ioctl_ior_nr!(GET_SHARED_MEMORY_SIZE_V1,    GH_IOCTL_TYPE_V1, 1, u64);
ioctl_iow_nr!(IOEVENTFD_V1,                 GH_IOCTL_TYPE_V1, 2, VirtioEventfd);
ioctl_iow_nr!(IRQFD_V1,                     GH_IOCTL_TYPE_V1, 3, VirtioIrqfd);
ioctl_iowr_nr!(WAIT_FOR_EVENT_V1,           GH_IOCTL_TYPE_V1, 4, VirtioEvent);
ioctl_iow_nr!(SET_DEVICE_FEATURES_V1,       GH_IOCTL_TYPE_V1, 5, VirtioDevFeatures);
ioctl_iow_nr!(SET_QUEUE_NUM_MAX_V1,         GH_IOCTL_TYPE_V1, 6, VirtioQueueMax);
ioctl_iow_nr!(SET_DEVICE_CONFIG_DATA_V1,    GH_IOCTL_TYPE_V1, 7, VirtioConfigData);
ioctl_iowr_nr!(GET_DRIVER_CONFIG_DATA_V1,   GH_IOCTL_TYPE_V1, 8, VirtioConfigData);
ioctl_iowr_nr!(GET_QUEUE_INFO_V1,           GH_IOCTL_TYPE_V1, 9, VirtioQueueInfo);
ioctl_iowr_nr!(GET_DRIVER_FEATURES_V1,      GH_IOCTL_TYPE_V1, 10, VirtioDriverFeatures);
ioctl_iowr_nr!(ACK_DRIVER_OK_V1,            GH_IOCTL_TYPE_V1, 11, u32);
ioctl_io_nr!(SET_APP_READY_V1,              GH_IOCTL_TYPE_V1, 12);
ioctl_iow_nr!(ACK_RESET_V1,                 GH_IOCTL_TYPE_V1, 13, VirtioAckReset);

/* vcpu ioctls */
ioctl_io_nr!(GH_VCPU_RUN,           GH_IOCTL_TYPE_V2, 0x80);
enum VmIoctl {
    IoEventFd,
    IrqFd,
    WaitForEvent,
    SetDeviceFeatures,
    SetQueueNumMax,
    SetDeviceConfigData,
    GetDriverConfigData,
    GetQueueInfo,
    GetDriverFeatures,
    AckDriverOk,
    AckReset,
    SetInputDeviceConfig,
    SetInputDeviceData
}

fn to_cmd(ioc: VmIoctl, version: u8) -> Result<u64, BackendError> {
    match version {
        2 => match ioc {
            VmIoctl::IoEventFd => Ok(IOEVENTFD_V2()),
            VmIoctl::IrqFd => Ok(IRQFD_V2()),
            VmIoctl::WaitForEvent => Ok(WAIT_FOR_EVENT_V2()),
            VmIoctl::SetDeviceFeatures => Ok(SET_DEVICE_FEATURES_V2()),
            VmIoctl::SetQueueNumMax => Ok(SET_QUEUE_NUM_MAX_V2()),
            VmIoctl::SetDeviceConfigData => Ok(SET_DEVICE_CONFIG_DATA_V2()),
            VmIoctl::GetDriverConfigData => Ok(GET_DRIVER_CONFIG_DATA_V2()),
            VmIoctl::GetQueueInfo => Ok(GET_QUEUE_INFO_V2()),
            VmIoctl::GetDriverFeatures => Ok(GET_DRIVER_FEATURES_V2()),
            VmIoctl::AckDriverOk => Ok(ACK_DRIVER_OK_V2()),
            VmIoctl::AckReset => Ok(ACK_RESET_V2()),
            VmIoctl::SetInputDeviceConfig => Ok(SET_INPUT_DEVICE_CONFIG_V2()),
            VmIoctl::SetInputDeviceData => Ok(SET_INPUT_DEVICE_DATA_V2()),
        }
        1 => match ioc {
            VmIoctl::IoEventFd => Ok(IOEVENTFD_V1()),
            VmIoctl::IrqFd => Ok(IRQFD_V1()),
            VmIoctl::WaitForEvent => Ok(WAIT_FOR_EVENT_V1()),
            VmIoctl::SetDeviceFeatures => Ok(SET_DEVICE_FEATURES_V1()),
            VmIoctl::SetQueueNumMax => Ok(SET_QUEUE_NUM_MAX_V1()),
            VmIoctl::SetDeviceConfigData => Ok(SET_DEVICE_CONFIG_DATA_V1()),
            VmIoctl::GetDriverConfigData => Ok(GET_DRIVER_CONFIG_DATA_V1()),
            VmIoctl::GetQueueInfo => Ok(GET_QUEUE_INFO_V1()),
            VmIoctl::GetDriverFeatures => Ok(GET_DRIVER_FEATURES_V1()),
            VmIoctl::AckDriverOk => Ok(ACK_DRIVER_OK_V1()),
            VmIoctl::AckReset => Ok(ACK_RESET_V1()),
            _ => Err(BackendError::StrError(String::from("Unsupported cmd"))),
        }
        _ => Err(BackendError::StrError(String::from("Unsupported driver variant."))),
    }
}

fn print_usage() {
    println!("qcrosvm [-l] [-s] [-c | --scmi=true,label=LABEL]
    [-d | --disk=IMAGE_FILE,label=LABEL[,rw=[true|false],sparse=[true|false],block_size=BYTES]]
    [-n | --net=true,label=LABEL,ip_addr=IP_ADDR,netmask=NETMASK,mac=MAC,tapname=TAP]
    [-i | --input=PATH,label=LABEL]
    [--vhost-user-hab SOCKET_PATH,device_id=DEVICE_ID,queue-num=QUEUE_NUM,label=LABEL]
    [--vhost-user-i2c SOCKET_PATH,label=LABEL]
    [--vhost-user-fs SOCKET_PATH,label=LABEL]
    [--vhost-user-scmi SOCKET_PATH,label=LABEL]
    [--vhost-user-frpc SOCKET_PATH,label=LABEL]
    [--vhost-user-ssr SOCKET_PATH,label=LABEL]
    [--vhost-user-gpio SOCKET_PATH,label=LABEL]
    [--console PATH,label=LABEL]
    [--vsock label=LABEL,cid=CONTEXT_ID]
    [--vhost-user-eavb SOCKET_PATH,label=LABEL]");
    #[cfg(feature = "vhost-user-generic")]
    println!("\t[--vhost-user-generic SOCKET_PATH,label=LABEL[,queue-num=QUEUE_NUM]]");
    println!("\t--vm=VMNAME");
    println!("\n[-l] or [--log=[level=trace|debug|info|warn|error],[type=ftrace|logcat|term]]");
    println!("Default logger level: info");
    println!("Default logger type: ftrace");
}

fn new_from_rawfd(ranges: &[(GuestAddress, u64)], fd: &RawFd) -> Result<GuestMemory, GuestMemoryError> {
    // Compute the memory alignment
    let pg_size = pagesize();
    let mut regions = Vec::new();
    let mut offset = 0;

    for range in ranges {
        if range.1 % pg_size as u64 != 0 {
            return Err(GuestMemoryError::MemoryNotAligned);
        }
        let file = Arc::new(unsafe { File::from_raw_fd(*fd) });
        let region = MemoryRegion::new_from_file(range.1, range.0, offset, file)
            .map_err(|e| {
                error!("{}", format!("failed to create mem region, addr:{}, size:{}. Err: {}", range.0, range.1, e));
                ()}).expect(&format!("{}:{}", file!(), line!()));
        regions.push(region);
        offset += range.1 as u64;
    }
    GuestMemory::from_regions(regions)
}

fn create_vm_mem_region(range: (GuestAddress, u64), fd: &RawFd, mem_ranges: &mut Vec<MemoryRegion>) -> std::result::Result<(), GuestMemoryError> {
    let pg_size = pagesize();

    if range.1 % pg_size as u64 != 0 {
        return Err(GuestMemoryError::MemoryNotAligned);
    }

    let file = Arc::new(unsafe { File::from_raw_fd(*fd) });
    let region = MemoryRegion::new_from_file(range.1, range.0, 0, file)
        .map_err(|e| {
                error!("{}", format!("failed to create mem region, addr:{}, size:{}. Err: {}", range.0, range.1, e));
                ()}).expect(&format!("{}:{}", file!(), line!()));
    mem_ranges.push(region);
    Ok(())
}

fn raw_fd_from_path(path: &Path) -> Result<RawFd, ()> {
    if !path.is_file() {
        return Err(());
    }
    let raw_fd = path
        .file_name()
        .and_then(|fd_osstr| fd_osstr.to_str())
        .and_then(|fd_str| fd_str.parse::<c_int>().ok())
        .ok_or(())?;
    validate_raw_fd(raw_fd).map_err(|_e| {()})
}

struct VirtioInputConfig {
    sel: u8,
    subsel: u8,
    size: u8,
    reserved: [u8; 5],
    payload: [u8; 128],
}

impl VirtioInputConfig {
    fn gen_config(mmio: &mut MmioDevice, sel: u8, subsel: u8) -> VirtioInputConfig {
        let mut sel_subsel: [u8; 2] = [0; 2];
        const len: usize = std::mem::size_of::<VirtioInputConfig>();
        let mut data: [u8; len] = [0; len];
        sel_subsel[0] = sel as u8;
        sel_subsel[1] = subsel as u8;
        mmio.write(VIRTIO_MMIO_INPUT_SEL as u64, &mut sel_subsel);
        /*
        * Read the device specific data after setting the sel and subsel,
        * 'data' contains VirtioInputConfig.
        */
        mmio.read(VIRTIO_MMIO_DEVICE_CONFIG as u64, &mut data);
        assert!((data[0] == sel) && (data[1] == subsel) && (data[2] <= 128),
            "failed to get config for input sel:{} subsel:{}!", sel, subsel);
        VirtioInputConfig {
            sel: data[0],
            subsel: data[1],
            size: data[2],
            reserved: [0u8; 5],
            payload: data[8..].try_into().unwrap(),
        }
    }
}

fn init_input_config(label: u32, mmio: &mut MmioDevice, sfd: &mut SafeDescriptor, driver_variant: u8) {

    let mut vinputdata: Vec<VirtioInputConfig> = Vec::new();

    let mut vinputdc = VirtioInputDeviceConfig {
        _label: label,
        _device_id: 0,
        _prop_bits: 0,
        _num_ev_types: 0,
        _num_abs_axes: 0,
        _reserved: 0,
    };

    let mut num_ev: u8 = 0;
    let mut num_abs: u8 = 0;

    let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_ID_NAME, 0);
    vinputdata.push(config);

    let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_ID_SERIAL, 0);
    vinputdata.push(config);

    let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_ID_DEVIDS, 0);
    assert!(config.size == 8, "device id size is not correct!");

    vinputdc._device_id = u64::from_le_bytes(config.payload[0..8].try_into().unwrap());
    debug!("{}", format!("device id is {:#x}", vinputdc._device_id));

    // get PROPBITS- 0x10
    let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_PROP_BITS, 0);
    assert!(config.size <= 4, "prop bits size is not correct!");

    vinputdc._prop_bits = u32::from_le_bytes(config.payload[0..4].try_into().unwrap());
    debug!("{}", format!("prop bits is {:#x}", vinputdc._prop_bits));

    // get EV_BITS - 0x11
    for ev_type in 0..EV_CNT as u8 {
        let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_EV_BITS, ev_type);
        if config.size != 0 {
            vinputdata.push(config);
            num_ev += 1;
        }
    }

    // get ABS_INFO - 0x12
    for abs_axis in 0..ABS_CNT as u8 {
        let config = VirtioInputConfig::gen_config(mmio, VIRTIO_INPUT_CFG_ABS_INFO, abs_axis);
        if config.size != 0 {
            vinputdata.push(config);
            num_abs += 1;
        }
    }

    debug!("{}", format!("evt types num is {}", num_ev));
    debug!("{}", format!("abs axes num is {}", num_abs));

    vinputdc._num_ev_types = num_ev;
    vinputdc._num_abs_axes = num_abs;

    // set input device config with ioctl
    let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetInputDeviceConfig, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut vinputdc)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    // set input device data with ioctl
    for config in vinputdata {
        if (config.size == 0) {
            warn!("[input<label={:#x}>]: data in sel<{}>/subsel<{}> is none, will not send to kernel",
                label, config.sel, config.subsel);
            continue;
        }
        let mut cdata = VirtioInputDeviceData {
            _label: label,
            _sel: config.sel,
            _subsel: config.subsel,
            _size: config.size,
            _reserved: [0u8; 5],
            _payload: config.payload.clone(),
        };

        let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetInputDeviceData, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut cdata)};
        assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());
    }
}

fn handle_device_reset(label: u32, sfd: &SafeDescriptor, mmio: &mut MmioDevice, driver_variant: u8) {

    let mut first_time = 1;
    let mut ackrst = VirtioAckReset {
        _label: label,
        _reserved: 0,
    };
    let status: u32 = DEVICE_RESET;
    let bytes = status.to_le_bytes();

    mmio.write(VIRTIO_MMIO_STATUS, &bytes);

    let mut idx = 0;

    for e in mmio.queue_evts() {
        let event_fd = VirtioEventfd {
            _label : label,
            _flags : ASSIGN_EVENTFD,
            _queue_num : idx,
            _fd : e.as_raw_descriptor(),
    };

    idx = idx + 1;

    let ret = unsafe { ioctl_with_ref(sfd,
                    to_cmd(VmIoctl::IoEventFd, driver_variant)
                    .expect(&format!("{}:{}", file!(), line!())),
                    &event_fd)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret,
                    io::Error::last_os_error());
    }
    if first_time == 1 {
        let ret = unsafe { ioctl_with_mut_ref(sfd,
                    to_cmd(VmIoctl::AckReset, driver_variant)
                    .expect(&format!("{}:{}", file!(), line!())),
                    &mut ackrst)};
        assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(),
                    ret, io::Error::last_os_error());
        first_time = 0;
    }
}

fn handle_driver_ok(label: u32, sfd: &SafeDescriptor, mmio: &mut MmioDevice, cspace: &mut Vec<u32>, driver_variant: u8) {

    let mut cdata = VirtioConfigData {
        _label: label,
        _config_size: 4096,
        _config_data: cspace.as_mut_ptr() as *mut c_char,
    };

    let label_copy = label;
    let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::GetDriverConfigData, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut cdata)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    let mut drv_feat = VirtioDriverFeatures {
        _label: label,
        _reserved: 0,
        _features_sel: 0,
        _features: 0,
    };

    let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::GetDriverFeatures, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut drv_feat)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    let bytes = 0x0u32.to_le_bytes();
    mmio.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, &bytes);

    let bytes = drv_feat._features.to_le_bytes();
    mmio.write(VIRTIO_MMIO_DRIVER_FEATURES, &bytes);

    drv_feat._features_sel = 1;

    let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::GetDriverFeatures, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut drv_feat)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    let bytes = 0x1u32.to_le_bytes();
    mmio.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, &bytes);

    let bytes = drv_feat._features.to_le_bytes();
    mmio.write(VIRTIO_MMIO_DRIVER_FEATURES_SEL, &bytes);

    let pos = mmio.get_num_queues();

    for queue in 0..pos as u32  {
        let mut qinfo = VirtioQueueInfo {
            _label: label,
            _queue_sel: queue,
            _queue_num: 0,
            _queue_ready: 0,
            _queue_desc: 0,
            _queue_driver: 0,
            _queue_device: 0,
        };

        let mut queue_addr: u32;
        let ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::GetQueueInfo, driver_variant)
                                            .expect(&format!("{}:{}", file!(), line!())), &mut qinfo)};
        assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

        let bytes = qinfo._queue_sel.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_SEL, &bytes);

        let bytes = qinfo._queue_num.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_NUM, &bytes);
        queue_addr = qinfo._queue_desc as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_DESC_LOW, &bytes);
        queue_addr = (qinfo._queue_desc >> 32) as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_DESC_HIGH, &bytes);
        queue_addr = qinfo._queue_driver as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_AVAIL_LOW, &bytes);
        queue_addr = (qinfo._queue_driver >> 32) as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_AVAIL_HIGH, &bytes);
        queue_addr = qinfo._queue_device as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_USED_LOW, &bytes);
        queue_addr = (qinfo._queue_device >> 32) as u32;

        let bytes = queue_addr.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_USED_HIGH, &bytes);

        let bytes = qinfo._queue_ready.to_le_bytes();
        mmio.write(VIRTIO_MMIO_QUEUE_READY, &bytes);
    }

    let bytes = cspace[VIRTIO_MMIO_STATUS_IDX as usize].to_le_bytes();
    mmio.write(VIRTIO_MMIO_STATUS, &bytes);

    let ret = unsafe { ioctl_with_val(sfd, to_cmd(VmIoctl::AckDriverOk, driver_variant)
                                    .expect(&format!("{}:{}", file!(), line!())), label_copy as u64)};
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());
}

fn handle_events(label: u32, sfd: SafeDescriptor, mmio: &mut MmioDevice, cspace: &mut Vec<u32>, driver_variant: u8) -> u32 {
    loop {
        let mut vevent  = VirtioEvent {
            _label: label,
            _event: 0,
            _event_data: 0,
            _reserved: 0,
        };

        let ret = unsafe { ioctl_with_mut_ref(&sfd, to_cmd(VmIoctl::WaitForEvent, driver_variant)
                                            .expect(&format!("{}:{}", file!(), line!())), &mut vevent)};
        assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

        match vevent._event {
            EVENT_DRIVER_OK => handle_driver_ok(label, &sfd, mmio, cspace, driver_variant),
            EVENT_INTERRUPT_ACK =>  {
                let status = vevent._event_data;
                let bytes = status.to_le_bytes();
                mmio.write(VIRTIO_MMIO_INTERRUPT_ACK, &bytes);
            }
            EVENT_RESET_RQST => handle_device_reset(label, &sfd, mmio, driver_variant),
            EVENT_APP_EXIT =>  {
                let bytes = 0x0u32.to_le_bytes();
                mmio.write(VIRTIO_MMIO_STATUS, &bytes);
                return 0;
            }
            _ => error!("{}", format!("Unexpected event {} received", vevent._event)),
        }
    }
}

fn read_banked_reg(mmio: &mut MmioDevice, sel: u32, offset_write: u64, offset_read: u64) -> u32 {
    let mut val: [u8; 4] = [0; 4];
    val[0] = sel as u8;
    mmio.write(offset_write as u64, &val);
    mmio.read(offset_read as u64, &mut val);
    u32::from_le_bytes(val)
}

fn init_config_space(config_space: &mut Vec<u32>, label: u32, mmio: &mut MmioDevice, sfd: &mut SafeDescriptor, driver_variant: u8) {
    let mut val: [u8; 4] = [0; 4];
    let mut reg: u32;
    let mut offset: usize = 0;
    let mut ret;

    // device config start from 0x100 to 0xfff, so the length is 0xf00(3840)
    let mut device_config: [u8; 3840] = [0; 3840];

    while offset < 256 {
        mmio.read(offset as u64, &mut val);
        reg = u32::from_le_bytes(val);
        config_space.push(reg);
        offset += 4;
    }

    mmio.read(offset as u64, &mut device_config);

    offset = 0;
    while offset < 3840 {
        val = device_config[offset..offset + 4].try_into().unwrap();
        reg = u32::from_le_bytes(val);
        config_space.push(reg);
        offset += 4;
    }

    let mut cdata = VirtioConfigData {
        _label: label,
        _config_size: 4096,
        _config_data: config_space.as_mut_ptr() as *mut c_char,
    };

    ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetDeviceConfigData, driver_variant)
                                    .expect(&format!("{}:{}", file!(), line!())), &mut cdata) };
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    let mut feat = VirtioDevFeatures {
        _label: label,
        _reserved: 0,
        _features_sel: 0,
        _features: 0,
    };

    feat._features = read_banked_reg(mmio, feat._features_sel, VIRTIO_MMIO_DEVICE_FEATURES_SEL, VIRTIO_MMIO_DEVICE_FEATURES);
    ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetDeviceFeatures, driver_variant)
                                    .expect(&format!("{}:{}", file!(), line!())), &mut feat) };
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    feat._features_sel = 1;
    feat._features = read_banked_reg(mmio, feat._features_sel, VIRTIO_MMIO_DEVICE_FEATURES_SEL, VIRTIO_MMIO_DEVICE_FEATURES);
    ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetDeviceFeatures, driver_variant)
        .expect(&format!("{}:{}", file!(), line!())), &mut feat) };
    assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());

    let pos = mmio.get_num_queues();

    for queue in 0..pos as u32  {
        let mut queue_max = VirtioQueueMax {
            _label: label,
            _reserved: 0,
            _queue_sel: queue,
            _queue_num_max: 0,
        };
        queue_max._queue_num_max = read_banked_reg(mmio, queue_max._queue_sel, VIRTIO_MMIO_QUEUE_SEL, VIRTIO_MMIO_QUEUE_NUM_MAX);
        ret = unsafe { ioctl_with_mut_ref(sfd, to_cmd(VmIoctl::SetQueueNumMax, driver_variant)
                                        .expect(&format!("{}:{}", file!(), line!())), &mut queue_max) };
        assert!(ret == 0, "{}:{}:ret={}, {}", file!(), line!(), ret, io::Error::last_os_error());
    }
}

fn set_minijail(policy: &str) -> Result<(), ()> {
    let mut jail = Minijail::new().map_err(|_| ())?;
    jail.no_new_privs();
    jail.parse_seccomp_filters(Path::new(policy)).map_err(|_| ())?;
    jail.use_seccomp_filter();
    // Jail the current process.
    jail.enter();
    Ok(())
}

fn parse_and_run(args: std::env::Args) -> Result<(), ()> {
    let arguments =
        &[
        Argument::short_value('d', "disk", "PATH,label=LABEL[,key=value[,key=value[,...]]", "Path to a disk image followed by comma-separated options.
                            Valid keys:
                            label=LABEL - Indicates the label associated with the virtual (disk)
                            sparse=BOOL - Indicates whether the disk should support the discard operation (default: true)
                            block_size=BYTES - Set the reported block size of the disk (default: 512)
                            rw - Sets the disk as read-writeable"),
        Argument::short_value('l', "log",
                            "[level=trace|debug|info|warn|error],[type=ftrace|logcat|term]",
                            "Logging Configurations. Default level: info, Default type: ftrace"),
        Argument::short_value('v', "vm", "VMNAME", "Virtual Machine Name"),
        Argument::short_flag('s', "sandbox", "Sandbox using minijail (default: disabled)."),
        Argument::flag("use-non-protected-virtio", "Use non protected VirtIO (no bounce buffers) (default: disabled)"),
        Argument::short_value('c', "scmi", "label=LABEL[,key=value[,key=value[,...]]", "Enable SCMI with the given label.
                            Valid keys:
                            label=LABEL - Indicates the label associated with the scmi virtio device"),
        Argument::value("vhost-user-scmi", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::value("vhost-user-i2c", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::value("vhost-user-fs", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        #[cfg(feature = "vhost-user-generic")]
        Argument::value("vhost-user-generic", "SOCKET_PATH", "label=LABEL[,queue-num=N]"),
        Argument::value("vhost-user-frpc", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::value("vhost-user-ssr", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::value("vhost-user-gpio", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::short_value('n',"net","label=LABEL[,key=value[,key=value[,key=value[,...]]]]","net device followed by comma-separated options.
                            Valid keys:
                            label=LABEL - Indicates the label associated with the virtual net dev
                            ip_addr=IP - IP address to assign to host tap interface
                            netmask=NETMASK - Netmask for VM subnet
                            mac=MAC - MAC address for VM,
                            tapName=TAP - Indicates VM name is provided for network configuration"),
        Argument::value("vhost-user-hab", "SOCKET_PATH", "label=LABEL[,key=value[,...]],device-id= device id  , queue-num = Number of queues"),
        Argument::short_value('i', "input", "PATH,label=LABEL[,key=value[,key=value[,...]]", "Path to a input device followed by comma-separated option label=LABEL."),
        Argument::value("console", "PATH,label=LABEL", "stdout or Path to a log file followed by comma-separated option label=LABEL"),
        Argument::value("vhost-user-gp", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        Argument::value("vsock", "label=LABEL[,key=value", "label=LABEL,cid=context-id"),
        Argument::value("vhost-user-eavb", "SOCKET_PATH", "label=LABEL[,key=value[,...]]"),
        ];

    let mut vm = VMBackend::new();
    let match_res = set_arguments(args, &arguments[..], |name, value| { vm.set_argument(name, value) });
    let _ = vm.set_logger();

    match match_res {
        Ok(()) => match vm.run_backend() {
            Ok(_) => {
                info!("backend exited normally");
                Ok(())
            }
            Err(_) => {
                Err(())
            }
        },
        Err(e) => {
            error!("{}", format!("Error parsing arguments {:?}", e));
            Err(())
        }
    }
}

fn backend_main() -> Result<(), ()> {
    if let Err(e) = syslog::init() {
        println!("failed to initialize syslog: {}", e);
        return Err(());
    }
    match env::var("KBDEV") {
        Ok(_) => panic_hook::set_panic_hook(),
        Err(_) => {},
    }
    let mut args = std::env::args();
    if args.next().is_none() {
        print_usage();
        return Err(());
    }
    return parse_and_run(args);
}

fn main() {
    std::process::exit(if backend_main().is_ok() { 0 } else { 1 });
}
