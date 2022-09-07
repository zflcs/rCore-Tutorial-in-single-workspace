use alloc::vec::Vec;

///Array of u8 slice that user communicate with os
pub struct UserBuffer {
  ///U8 vec
  pub buffers: Vec<&'static mut [u8]>,
}

impl UserBuffer {
  ///Create a `UserBuffer` by parameter
  pub fn new(buffers: Vec<&'static mut [u8]>) -> Self {
      Self { buffers }
  }
  ///Length of `UserBuffer`
  pub fn len(&self) -> usize {
      let mut total: usize = 0;
      for b in self.buffers.iter() {
          total += b.len();
      }
      total
  }
}

/// File trait
pub trait File: Send + Sync {
  /// If readable
  fn readable(&self) -> bool;
  /// If writable
  fn writable(&self) -> bool;
  /// Read file to `UserBuffer`
  fn read(&self, buf: UserBuffer) -> usize;
  /// Write `UserBuffer` to file
  fn write(&self, buf: UserBuffer) -> usize;
}

pub struct Stdin;
pub struct Stdout;