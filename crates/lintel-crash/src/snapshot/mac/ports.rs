//! Owns the rights and validates the two fixed messages in the snapshot handoff.
#![allow(unsafe_code)]

use std::{ffi::CStr, mem::size_of, time::Duration};

use anyhow::{Result, ensure};
use mach2::{
  bootstrap,
  kern_return::{KERN_SUCCESS, kern_return_t},
  mach_port, message as msg,
  port::{MACH_PORT_DEAD, MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t},
  task::task_get_special_port,
  task_special_ports::TASK_BOOTSTRAP_PORT,
  traps::mach_task_self,
};

use super::{SnapshotError, TOKEN_BYTES};

#[link(name = "bsm")]
unsafe extern "C" {
  fn audit_token_to_pid(token: msg::audit_token_t) -> libc::pid_t;
}

unsafe extern "C" {
  fn pid_for_task(task: mach_port_t, pid: *mut libc::pid_t) -> kern_return_t;
}

pub(super) struct ReceiveRight(mach_port_t);

impl ReceiveRight {
  pub(super) fn check_in(name: &CStr) -> Result<Self> {
    let bootstrap = SendRight::special(TASK_BOOTSTRAP_PORT)?;
    let mut port = MACH_PORT_NULL;
    // SAFETY: The bootstrap send right is live, the service name is NUL-terminated and `port`
    // is writable storage for the newly transferred receive right.
    let result = unsafe { bootstrap::bootstrap_check_in(bootstrap.0, name.as_ptr(), &mut port) };
    check_port(result, port, "snapshot bootstrap registration").map(Self)
  }

  pub(super) fn allocate() -> Result<Self> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `port` is writable storage for a new receive right in the calling task.
    let result = unsafe { mach_port::mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
    check_port(result, port, "snapshot reply port allocation").map(Self)
  }

  pub(super) fn name(&self) -> mach_port_t {
    self.0
  }

  pub(super) fn receive(
    &self,
    id: i32,
    pid: libc::pid_t,
    token: &[u8; TOKEN_BYTES],
    timeout: Duration,
  ) -> Result<Option<SendRight>> {
    let mut received = ReceivedMessage::default();
    // `MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)` from mach/message.h.
    let audit_trailer = (msg::MACH_RCV_TRAILER_AUDIT << 24) as msg::mach_msg_option_t;
    // SAFETY: The header begins a contiguous buffer covering the entire advertised receive
    // length, including the trailer. `self` owns the live receive right for this operation.
    let result = unsafe {
      msg::mach_msg(
        std::ptr::from_mut(&mut received).cast(),
        msg::MACH_RCV_MSG | msg::MACH_RCV_TIMEOUT | audit_trailer,
        0,
        RECEIVE_BYTES,
        self.0,
        milliseconds(timeout),
        MACH_PORT_NULL,
      )
    };
    // On a body error, the kernel may have still copied out some descriptors. The message's
    // destructor releases every resource the kernel delivered, including rejected messages.
    received.owns_rights =
      result == msg::MACH_MSG_SUCCESS || (result & !msg::MACH_MSG_MASK) == msg::MACH_RCV_BODY_ERROR;
    if result == msg::MACH_RCV_TIMED_OUT {
      return Ok(None);
    }
    ensure!(
      result == msg::MACH_MSG_SUCCESS,
      "cannot receive snapshot Mach message ({result:#x})"
    );
    let payload = &received.payload;
    ensure!(
      payload.header.msgh_size == PAYLOAD_BYTES
        && payload.header.msgh_bits & msg::MACH_MSGH_BITS_COMPLEX != 0
        && payload.header.msgh_remote_port == MACH_PORT_NULL
        && payload.header.msgh_voucher_port == MACH_PORT_NULL
        && payload.header.msgh_id == id
        && payload.body.msgh_descriptor_count == 1
        && payload.port.type_ == msg::MACH_MSG_PORT_DESCRIPTOR as u8
        && payload.port.disposition == msg::MACH_MSG_TYPE_MOVE_SEND as u8
        && valid_port(payload.port.name)
        && received.trailer.msgh_trailer_type == msg::MACH_MSG_TRAILER_FORMAT_0
        && received.trailer.msgh_trailer_size as usize == size_of::<msg::mach_msg_audit_trailer_t>(),
      "invalid snapshot Mach message"
    );
    // SAFETY: The kernel supplied this exact validated audit trailer. libbsm takes its token by value.
    let sender = unsafe { audit_token_to_pid(received.trailer.msgh_audit) };
    ensure!(
      sender == pid && &payload.token == token,
      "snapshot Mach message could not be authenticated"
    );
    let name = std::mem::replace(&mut received.payload.port.name, MACH_PORT_NULL);
    Ok(Some(SendRight(name)))
  }
}

impl Drop for ReceiveRight {
  fn drop(&mut self) {
    // SAFETY: This is the sole owner of this receive right. Discard queued messages with it.
    unsafe { mach_port::mach_port_mod_refs(mach_task_self(), self.0, MACH_PORT_RIGHT_RECEIVE, -1) };
  }
}

pub(super) struct SendRight(mach_port_t);

impl SendRight {
  fn special(which: i32) -> Result<Self> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `port` is writable storage for a send right from the calling task's special ports.
    let result = unsafe { task_get_special_port(mach_task_self(), which, &mut port) };
    check_port(result, port, "snapshot special port lookup").map(Self)
  }

  pub(super) fn send_self_task(&self, id: i32, token: &[u8; TOKEN_BYTES], timeout: Duration) -> Result<()> {
    // SAFETY: The current task's cached control port is borrowed and COPY_SEND keeps its
    // reference in this process. XNU's dedicated task-read right is immovable, so it cannot
    // be sent over this channel. Only the authenticated child is given this capability.
    self.send(
      id,
      unsafe { mach_task_self() },
      msg::MACH_MSG_TYPE_COPY_SEND,
      token,
      timeout,
    )
  }

  pub(super) fn look_up(name: &CStr) -> Result<Self> {
    let bootstrap = Self::special(TASK_BOOTSTRAP_PORT)?;
    let mut port = MACH_PORT_NULL;
    // SAFETY: The bootstrap send right is live, the service name is NUL-terminated and `port`
    // is writable storage for the newly copied send right.
    let result = unsafe { bootstrap::bootstrap_look_up(bootstrap.0, name.as_ptr(), &mut port) };
    check_port(result, port, "snapshot bootstrap lookup").map(Self)
  }

  pub(super) fn name(&self) -> mach_port_t {
    self.0
  }

  pub(super) fn pid(&self) -> Result<libc::pid_t> {
    let mut pid = 0;
    // SAFETY: `pid` is writable storage; the kernel validates whether the right names a task.
    let result = unsafe { pid_for_task(self.0, &mut pid) };
    ensure!(result == KERN_SUCCESS, "cannot identify snapshot task ({result:#x})");
    Ok(pid)
  }

  pub(super) fn send(
    &self,
    id: i32,
    port: mach_port_t,
    disposition: msg::mach_msg_type_name_t,
    token: &[u8; TOKEN_BYTES],
    timeout: Duration,
  ) -> Result<()> {
    debug_assert!(matches!(
      disposition,
      msg::MACH_MSG_TYPE_COPY_SEND | msg::MACH_MSG_TYPE_MAKE_SEND
    ));
    let mut payload = Payload {
      header: msg::mach_msg_header_t {
        msgh_bits: msg::MACH_MSGH_BITS_COMPLEX | msg::MACH_MSG_TYPE_COPY_SEND,
        msgh_size: PAYLOAD_BYTES,
        msgh_remote_port: self.0,
        msgh_id: id,
        ..Default::default()
      },
      body: msg::mach_msg_body_t {
        msgh_descriptor_count: 1,
      },
      port: msg::mach_msg_port_descriptor_t::new(port, disposition),
      token: *token,
    };
    // SAFETY: The header begins a contiguous buffer of `PAYLOAD_BYTES`. Both dispositions
    // keep caller-owned rights in place regardless of delivery success.
    let result = unsafe {
      msg::mach_msg(
        std::ptr::from_mut(&mut payload).cast(),
        msg::MACH_SEND_MSG | msg::MACH_SEND_TIMEOUT,
        PAYLOAD_BYTES,
        0,
        MACH_PORT_NULL,
        milliseconds(timeout),
        MACH_PORT_NULL,
      )
    };
    if result == msg::MACH_SEND_TIMED_OUT {
      return Err(SnapshotError::TimedOut.into());
    }
    ensure!(
      result == msg::MACH_MSG_SUCCESS,
      "cannot send snapshot Mach message ({result:#x})"
    );
    Ok(())
  }
}

impl Drop for SendRight {
  fn drop(&mut self) {
    // SAFETY: This wrapper owns one reference to a send right or its corresponding dead name.
    unsafe { mach_port::mach_port_deallocate(mach_task_self(), self.0) };
  }
}

#[derive(Default)]
#[repr(C)]
struct Payload {
  header: msg::mach_msg_header_t,
  body: msg::mach_msg_body_t,
  port: msg::mach_msg_port_descriptor_t,
  token: [u8; TOKEN_BYTES],
}

#[derive(Default)]
#[repr(C)]
struct ReceivedMessage {
  payload: Payload,
  trailer: msg::mach_msg_audit_trailer_t,
  owns_rights: bool,
}

impl Drop for ReceivedMessage {
  fn drop(&mut self) {
    if self.owns_rights {
      // SAFETY: XNU populated the message and its descriptor table. The validated port, if
      // moved into a SendRight, has already been nulled so it cannot be released twice.
      unsafe { msg::mach_msg_destroy(std::ptr::from_mut(self).cast()) };
    }
  }
}

const PAYLOAD_BYTES: u32 = size_of::<Payload>() as u32;
const RECEIVE_BYTES: u32 = PAYLOAD_BYTES + size_of::<msg::mach_msg_audit_trailer_t>() as u32;

fn milliseconds(duration: Duration) -> u32 {
  duration.as_millis().clamp(1, u128::from(u32::MAX)) as u32
}

fn valid_port(port: mach_port_t) -> bool {
  port != MACH_PORT_NULL && port != MACH_PORT_DEAD
}

fn check_port(result: kern_return_t, port: mach_port_t, operation: &str) -> Result<mach_port_t> {
  ensure!(
    result == KERN_SUCCESS && valid_port(port),
    "{operation} failed ({result:#x})"
  );
  Ok(port)
}
