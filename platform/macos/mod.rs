// Copyright 2015 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use platform::macos::mach_sys::{kern_return_t, mach_msg_body_t, mach_msg_header_t};
use platform::macos::mach_sys::{mach_msg_port_descriptor_t, mach_msg_timeout_t, mach_port_right_t};
use platform::macos::mach_sys::{mach_port_t, mach_task_self_};

use libc::{self, c_char, size_t};
use rand::{self, Rng};
use std::cell::Cell;
use std::ffi::CString;
use std::mem;
use std::ptr;
use std::slice;

mod mach_sys;

/// The size that we preallocate on the stack to receive messages. If the message is larger than
/// this, we retry and spill to the heap.
const SMALL_MESSAGE_SIZE: usize = 4096;

/// A string to prepend to our bootstrap ports.
static BOOTSTRAP_PREFIX: &'static str = "org.rust-lang.ipc-channel.";

const BOOTSTRAP_SUCCESS: kern_return_t = 0;
const BOOTSTRAP_NAME_IN_USE: kern_return_t = 1101;
const KERN_SUCCESS: kern_return_t = 0;
const KERN_INVALID_RIGHT: kern_return_t = 17;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const MACH_MSG_SUCCESS: kern_return_t = 0;
const MACH_MSG_TIMEOUT_NONE: mach_msg_timeout_t = 0;
const MACH_MSG_TYPE_MOVE_RECEIVE: u8 = 16;
const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
const MACH_MSG_TYPE_COPY_SEND: u8 = 19;
const MACH_MSG_TYPE_MAKE_SEND: u8 = 20;
const MACH_MSG_TYPE_PORT_SEND: u8 = MACH_MSG_TYPE_MOVE_SEND;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x80000000;
const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_PORT_SET: mach_port_right_t = 3;
const MACH_PORT_RIGHT_RECEIVE: mach_port_right_t = 1;
const MACH_PORT_RIGHT_SEND: mach_port_right_t = 0;
const MACH_SEND_MSG: i32 = 1;
const MACH_RCV_MSG: i32 = 2;
const MACH_RCV_LARGE: i32 = 4;
const MACH_RCV_TOO_LARGE: i32 = 0x10004004;
const TASK_BOOTSTRAP_PORT: i32 = 4;

#[allow(non_camel_case_types)]
type name_t = *const c_char;

pub fn channel() -> Result<(MachSender, MachReceiver),kern_return_t> {
    let receiver = try!(MachReceiver::new());
    let sender = try!(receiver.sender());
    Ok((sender, receiver))
}

#[derive(PartialEq, Debug)]
pub struct MachReceiver {
    port: Cell<mach_port_t>,
}

impl Drop for MachReceiver {
    fn drop(&mut self) {
        let port = self.port.get();
        if port != MACH_PORT_NULL {
            unsafe {
                assert!(mach_sys::mach_port_mod_refs(mach_task_self(),
                                                     port,
                                                     MACH_PORT_RIGHT_RECEIVE,
                                                     -1) == KERN_SUCCESS);
            }
        }
    }
}

impl MachReceiver {
    fn new() -> Result<MachReceiver,kern_return_t> {
        let mut port: mach_port_t = 0;
        let os_result = unsafe {
            mach_sys::mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port)
        };
        if os_result == KERN_SUCCESS {
            Ok(MachReceiver::from_name(port))
        } else {
            Err(os_result)
        }
    }

    fn from_name(port: mach_port_t) -> MachReceiver {
        MachReceiver {
            port: Cell::new(port),
        }
    }

    fn consume_port(&self) -> mach_port_t {
        let port = self.port.get();
        debug_assert!(port != MACH_PORT_NULL);
        self.port.set(MACH_PORT_NULL);
        port
    }

    pub fn consume(&self) -> MachReceiver {
        MachReceiver::from_name(self.consume_port())
    }

    fn sender(&self) -> Result<MachSender,kern_return_t> {
        let port = self.port.get();
        debug_assert!(port != MACH_PORT_NULL);
        unsafe {
            let (mut right, mut acquired_right) = (0, 0);
            let os_result = mach_sys::mach_port_extract_right(mach_task_self(),
                                                              port,
                                                              MACH_MSG_TYPE_MAKE_SEND as u32,
                                                              &mut right,
                                                              &mut acquired_right);
            if os_result == KERN_SUCCESS {
                debug_assert!(acquired_right == MACH_MSG_TYPE_PORT_SEND as u32);
                Ok(MachSender::from_name(right))
            } else {
                Err(os_result)
            }
        }
    }

    fn register_bootstrap_name(&self) -> Result<String,kern_return_t> {
        let port = self.port.get();
        debug_assert!(port != MACH_PORT_NULL);
        unsafe {
            let mut bootstrap_port = 0;
            let os_result = mach_sys::task_get_special_port(mach_task_self(),
                                                            TASK_BOOTSTRAP_PORT,
                                                            &mut bootstrap_port);
            if os_result != KERN_SUCCESS {
                return Err(os_result)
            }


            // FIXME(pcwalton): Does this leak?
            let (mut right, mut acquired_right) = (0, 0);
            let os_result = mach_sys::mach_port_extract_right(mach_task_self(),
                                                              port,
                                                              MACH_MSG_TYPE_MAKE_SEND as u32,
                                                              &mut right,
                                                              &mut acquired_right);
            if os_result != KERN_SUCCESS {
                return Err(os_result)
            }
            debug_assert!(acquired_right == MACH_MSG_TYPE_PORT_SEND as u32);

            let mut os_result;
            let mut name;
            loop {
                name = format!("{}{}", BOOTSTRAP_PREFIX, rand::thread_rng().gen::<i64>());
                let c_name = CString::new(name.clone()).unwrap();
                os_result = bootstrap_register2(bootstrap_port, c_name.as_ptr(), right, 0);
                if os_result == BOOTSTRAP_NAME_IN_USE {
                    continue
                }
                if os_result != BOOTSTRAP_SUCCESS {
                    return Err(os_result)
                }
                break
            }
            Ok(name)
        }
    }

    fn unregister_global_name(name: String) -> Result<(),kern_return_t> {
        unsafe {
            let mut bootstrap_port = 0;
            let os_result = mach_sys::task_get_special_port(mach_task_self(),
                                                            TASK_BOOTSTRAP_PORT,
                                                            &mut bootstrap_port);
            if os_result != KERN_SUCCESS {
                return Err(os_result)
            }

            let c_name = CString::new(name).unwrap();
            let os_result = bootstrap_register2(bootstrap_port,
                                                c_name.as_ptr(),
                                                MACH_PORT_NULL,
                                                0);
            if os_result == BOOTSTRAP_SUCCESS {
                Ok(())
            } else {
                Err(os_result)
            }
        }
    }

    pub fn recv(&self) -> Result<(Vec<u8>, Vec<OpaqueMachChannel>),kern_return_t> {
        recv(self.port.get()).map(|(_, data, channels)| (data, channels))
    }
}

#[derive(PartialEq, Debug)]
pub struct MachSender {
    port: mach_port_t,
}

impl Drop for MachSender {
    fn drop(&mut self) {
        unsafe {
            let error = mach_sys::mach_port_mod_refs(mach_task_self(),
                                                     self.port,
                                                     MACH_PORT_RIGHT_SEND,
                                                     -1);
            // `KERN_INVALID_RIGHT` is returned if (as far as I can tell) the receiver already shut
            // down. This is fine.
            if error != KERN_SUCCESS && error != KERN_INVALID_RIGHT {
                panic!("mach_port_mod_refs(-1, {}) failed: {:08x}", self.port, error)
            }
        }
    }
}

impl Clone for MachSender {
    fn clone(&self) -> MachSender {
        unsafe {
            assert!(mach_sys::mach_port_mod_refs(mach_task_self(),
                                                 self.port,
                                                 MACH_PORT_RIGHT_SEND,
                                                 1) == KERN_SUCCESS);
        }
        MachSender {
            port: self.port,
        }
    }
}

impl MachSender {
    fn from_name(port: mach_port_t) -> MachSender {
        MachSender {
            port: port,
        }
    }

    pub fn connect(name: String) -> Result<MachSender,kern_return_t> {
        unsafe {
            let mut bootstrap_port = 0;
            let os_result = mach_sys::task_get_special_port(mach_task_self(),
                                                            TASK_BOOTSTRAP_PORT,
                                                            &mut bootstrap_port);
            if os_result != KERN_SUCCESS {
                return Err(os_result)
            }

            let mut port = 0;
            let c_name = CString::new(name).unwrap();
            let os_result = bootstrap_look_up(bootstrap_port, c_name.as_ptr(), &mut port);
            if os_result == BOOTSTRAP_SUCCESS {
                Ok(MachSender::from_name(port))
            } else {
                Err(os_result)
            }
        }
    }

    pub fn send(&self, data: &[u8], ports: Vec<MachChannel>) -> Result<(),kern_return_t> {
        unsafe {
            let size = Message::size_of(data.len(), ports.len());
            let message = libc::malloc(size as size_t) as *mut Message;
            (*message).header.msgh_bits = (MACH_MSG_TYPE_COPY_SEND as u32) |
                MACH_MSGH_BITS_COMPLEX;
            (*message).header.msgh_size = size as u32;
            (*message).header.msgh_local_port = MACH_PORT_NULL;
            (*message).header.msgh_remote_port = self.port;
            (*message).header.msgh_reserved = 0;
            (*message).header.msgh_id = 0;
            (*message).body.msgh_descriptor_count = ports.len() as u32;
            let mut port_descriptor_dest = message.offset(1) as *mut mach_msg_port_descriptor_t;
            for outgoing_port in ports.into_iter() {
                (*port_descriptor_dest).name = outgoing_port.port();
                (*port_descriptor_dest).pad1 = 0;

                // FIXME(pcwalton): MOVE_SEND maybe?
                (*port_descriptor_dest).disposition = match outgoing_port {
                    MachChannel::Sender(_) => MACH_MSG_TYPE_COPY_SEND,
                    MachChannel::Receiver(_) => MACH_MSG_TYPE_MOVE_RECEIVE,
                };

                (*port_descriptor_dest).type_ = MACH_MSG_PORT_DESCRIPTOR;
                port_descriptor_dest = port_descriptor_dest.offset(1);
                mem::forget(outgoing_port);
            }

            // Zero out the last word for paranoia's sake.
            *((message as *mut u8).offset(size as isize - 4) as *mut u32) = 0;

            let data_dest = port_descriptor_dest as *mut u8;
            ptr::copy_nonoverlapping(data.as_ptr(), data_dest, data.len());

            let mut ptr = message as *const u32;
            let end = (message as *const u8).offset(size as isize) as *const u32;
            while ptr < end {
                ptr = ptr.offset(1);
            }

            let os_result = mach_sys::mach_msg(message as *mut _,
                                               MACH_SEND_MSG,
                                               (*message).header.msgh_size,
                                               0,
                                               MACH_PORT_NULL,
                                               MACH_MSG_TIMEOUT_NONE,
                                               MACH_PORT_NULL);
            if os_result != MACH_MSG_SUCCESS {
                return Err(os_result)
            }
            libc::free(message as *mut _);
            Ok(())
        }
    }
}

pub enum MachChannel {
    Sender(MachSender),
    Receiver(MachReceiver),
}

impl MachChannel {
    fn port(&self) -> mach_port_t {
        match *self {
            MachChannel::Sender(ref sender) => sender.port,
            MachChannel::Receiver(ref receiver) => receiver.port.get(),
        }
    }
}

#[derive(PartialEq, Debug)]
pub struct OpaqueMachChannel {
    port: mach_port_t,
}

impl Drop for OpaqueMachChannel {
    fn drop(&mut self) {
        // Make sure we don't leak!
        debug_assert!(self.port == MACH_PORT_NULL);
    }
}

impl OpaqueMachChannel {
    fn from_name(name: mach_port_t) -> OpaqueMachChannel {
        OpaqueMachChannel {
            port: name,
        }
    }

    pub fn to_sender(&mut self) -> MachSender {
        MachSender {
            port: mem::replace(&mut self.port, MACH_PORT_NULL),
        }
    }

    pub fn to_receiver(&mut self) -> MachReceiver {
        MachReceiver::from_name(mem::replace(&mut self.port, MACH_PORT_NULL))
    }
}

pub struct MachReceiverSet {
    port: Cell<mach_port_t>,
}

impl MachReceiverSet {
    pub fn new() -> Result<MachReceiverSet,kern_return_t> {
        let mut port: mach_port_t = 0;
        let os_result = unsafe {
            mach_sys::mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_PORT_SET, &mut port)
        };
        if os_result == KERN_SUCCESS {
            Ok(MachReceiverSet {
                port: Cell::new(port),
            })
        } else {
            Err(os_result)
        }
    }

    pub fn add(&self, receiver: MachReceiver) -> Result<i64,kern_return_t> {
        let receiver_port = receiver.consume_port();
        let os_result = unsafe {
            mach_sys::mach_port_move_member(mach_task_self(), receiver_port, self.port.get())
        };
        if os_result == KERN_SUCCESS {
            Ok(receiver_port as i64)
        } else {
            Err(os_result)
        }
    }

    pub fn recv(&self) -> Result<(i64, Vec<u8>, Vec<OpaqueMachChannel>),kern_return_t> {
        recv(self.port.get()).map(|(port, data, channels)| (port as i64, data, channels))
    }
}

fn recv(port: mach_port_t)
        -> Result<(mach_port_t, Vec<u8>, Vec<OpaqueMachChannel>),kern_return_t> {
    debug_assert!(port != MACH_PORT_NULL);
    unsafe {
        let mut buffer = [0; SMALL_MESSAGE_SIZE];
        let allocated_buffer = None;
        setup_receive_buffer(&mut buffer, port);
        let mut message = &mut buffer[0] as *mut _ as *mut Message;
        match mach_sys::mach_msg(message as *mut _,
                                 MACH_RCV_MSG | MACH_RCV_LARGE,
                                 0,
                                 (*message).header.msgh_size,
                                 port,
                                 MACH_MSG_TIMEOUT_NONE,
                                 MACH_PORT_NULL) {
            MACH_RCV_TOO_LARGE => {
                // For some reason the size reported by the kernel is too small by 8. Why?!
                let actual_size = (*message).header.msgh_size + 8;
                let allocated_buffer = Some(libc::malloc(actual_size as size_t));
                setup_receive_buffer(slice::from_raw_parts_mut(
                                        allocated_buffer.unwrap() as *mut u8,
                                        actual_size as usize),
                                     port);
                message = allocated_buffer.unwrap() as *mut Message;
                match mach_sys::mach_msg(message as *mut _,
                                         MACH_RCV_MSG | MACH_RCV_LARGE,
                                         0,
                                         actual_size,
                                         port,
                                         MACH_MSG_TIMEOUT_NONE,
                                         MACH_PORT_NULL) {
                    MACH_MSG_SUCCESS => {}
                    os_result => return Err(os_result),
                }
            }
            MACH_MSG_SUCCESS => {}
            os_result => return Err(os_result),
        }

        let mut ports = Vec::new();
        let mut port_descriptor = message.offset(1) as *mut mach_msg_port_descriptor_t;
        for _ in 0..(*message).body.msgh_descriptor_count {
            ports.push(OpaqueMachChannel::from_name((*port_descriptor).name));
            port_descriptor = port_descriptor.offset(1);
        }

        let payload_ptr = port_descriptor as *mut u8;
        let payload_size = message as usize + ((*message).header.msgh_size as usize) -
            (port_descriptor as usize);
        let payload = slice::from_raw_parts(payload_ptr, payload_size).to_vec();

        if let Some(allocated_buffer) = allocated_buffer {
            libc::free(allocated_buffer)
        }

        Ok(((*message).header.msgh_local_port, payload, ports))
    }
}

pub struct MachOneShotServer {
    receiver: Option<MachReceiver>,
    name: String,
}

impl Drop for MachOneShotServer {
    fn drop(&mut self) {
        drop(MachReceiver::unregister_global_name(mem::replace(&mut self.name, String::new())));
    }
}

impl MachOneShotServer {
    pub fn new() -> Result<(MachOneShotServer, String),kern_return_t> {
        let receiver = try!(MachReceiver::new());
        let name = try!(receiver.register_bootstrap_name());
        Ok((MachOneShotServer {
            receiver: Some(receiver),
            name: name.clone(),
        }, name))
    }

    pub fn accept(mut self)
                  -> Result<(MachReceiver, Vec<u8>, Vec<OpaqueMachChannel>),kern_return_t> {
        let (bytes, channels) = try!(self.receiver.as_mut().unwrap().recv());
        Ok((mem::replace(&mut self.receiver, None).unwrap(), bytes, channels))
    }
}

unsafe fn setup_receive_buffer(buffer: &mut [u8], port_name: mach_port_t) {
    let message: *mut mach_msg_header_t = mem::transmute(&buffer[0]);
    (*message).msgh_local_port = port_name;
    (*message).msgh_size = buffer.len() as u32
}

unsafe fn mach_task_self() -> mach_port_t {
    mach_task_self_
}

#[repr(C)]
struct Message {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
}

impl Message {
    fn size_of(data_length: usize, port_length: usize) -> usize {
        let mut size = mem::size_of::<Message>() +
            mem::size_of::<mach_msg_port_descriptor_t>() * port_length + data_length;

        // Round up to the next 4 bytes.
        if (size & 0x3) != 0 {
            size = (size & !0x3) + 4;
        }

        size
    }
}

extern {
    fn bootstrap_register2(bp: mach_port_t, service_name: name_t, sp: mach_port_t, flags: u64)
                           -> kern_return_t;
    fn bootstrap_look_up(bp: mach_port_t, service_name: name_t, sp: *mut mach_port_t)
                         -> kern_return_t;
}

