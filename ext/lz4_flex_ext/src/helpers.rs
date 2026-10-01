use serde::{Deserialize, Serialize};
use std::{cell::Cell, ffi::c_void, marker::PhantomData, mem::MaybeUninit, ptr::null_mut};

use magnus::{
    encoding::{EncodingCapable, Index},
    rb_sys::{protect, AsRawValue},
    value::ReprValue,
    Error, RString, Ruby, TryConvert, Value,
};
use rb_sys::{
    rb_gc_guard, rb_str_locktmp, rb_str_modify_expand, rb_str_set_len, rb_str_unlocktmp,
    rb_thread_call_without_gvl, stable_api, StableApiDefinition, RSTRING_PTR,
};

use crate::base_error_class;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum Encoding {
    Utf8 = 0,
    Binary = 1,
    UsAscii = 2,
}

impl Encoding {
    pub(crate) fn encindex(self) -> Index {
        let ruby = unsafe { Ruby::get_unchecked() };

        match self {
            Self::Utf8 => ruby.utf8_encindex(),
            Self::Binary => ruby.ascii8bit_encindex(),
            Self::UsAscii => ruby.usascii_encindex(),
        }
    }

    pub(crate) const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Utf8,
            1 => Self::Binary,
            2 => Self::UsAscii,
            _ => unreachable!(),
        }
    }
}

impl TryFrom<RString> for Encoding {
    type Error = Error;

    fn try_from(string: RString) -> Result<Self, Self::Error> {
        let ruby = unsafe { Ruby::get_unchecked() };

        let enc = string.enc_get();

        if enc == ruby.utf8_encindex() {
            return Ok(Encoding::Utf8);
        }

        if enc == ruby.ascii8bit_encindex() {
            return Ok(Encoding::Binary);
        }

        if enc == ruby.usascii_encindex() {
            return Ok(Encoding::UsAscii);
        }

        Err(Error::new(
            base_error_class(),
            "unsupported encoding for string, please use Lz4Flex.compress_block instead".to_owned(),
        ))
    }
}

#[derive(Debug)]
pub struct LockedRString<'a> {
    string: RString,
    locked: bool,
    _phantom: PhantomData<&'a ()>,
}

impl<'a> LockedRString<'a> {
    pub(crate) fn new(string: RString) -> Self {
        let api = stable_api::get_default();
        let is_frozen = unsafe { api.frozen_p(string.as_raw()) };

        if !is_frozen {
            unsafe { rb_str_locktmp(string.as_raw()) };
        }

        Self {
            string,
            locked: !is_frozen,
            _phantom: PhantomData,
        }
    }

    pub(crate) fn as_slice(&self) -> &'a [u8] {
        unsafe {
            std::slice::from_raw_parts(
                RSTRING_PTR(self.string.as_raw()) as *const u8,
                self.string.len(),
            )
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.string.len()
    }

    pub(crate) fn encoding(&self) -> Result<Encoding, Error> {
        Encoding::try_from(self.string)
    }
}

impl Drop for LockedRString<'_> {
    fn drop(&mut self) {
        if self.locked {
            unsafe { rb_str_unlocktmp(self.string.as_raw()) };
        }
        // Keep the value alive while we hold this RString
        let _ = rb_gc_guard!(self.string.as_raw());
    }
}

impl TryConvert for LockedRString<'_> {
    fn try_convert(val: Value) -> Result<Self, magnus::Error> {
        let ruby = unsafe { Ruby::get_unchecked() };
        let rstring: RString = RString::from_value(val).ok_or(magnus::Error::new(
            ruby.exception_type_error(),
            format!("expected String, got {}", val.class()),
        ))?;
        Ok(Self::new(rstring))
    }
}

#[derive(Debug)]
pub struct RStringMut<'a> {
    inner: RString,
    capa: Cell<usize>,
    // Used to safely borrow the underlying rstring buffer by associating it with the lifetime of
    // the RStringMut
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> RStringMut<'a> {
    pub(crate) fn zeroed(size: usize) -> Result<Self, Error> {
        let ruby = unsafe { Ruby::get_unchecked() };
        if size > std::os::raw::c_long::MAX as usize {
            return Err(Error::new(
                ruby.exception_range_error(),
                "output capacity exceeds the maximum string size",
            ));
        }

        let mut output = None;
        protect(|| {
            output = Some(Self::buf_new(size));
            rb_sys::Qnil as _
        })?;
        let output = output.expect("successful allocation returns a buffer");
        // lz4_flex takes initialized slices, while rb_str_buf_new leaves bytes uninitialized.
        unsafe { std::ptr::write_bytes(RSTRING_PTR(output.inner.as_raw()) as *mut u8, 0, size) };
        Ok(output)
    }

    pub(crate) fn buf_new(size: usize) -> Self {
        let ruby = unsafe { Ruby::get_unchecked() };
        let string = ruby.str_buf_new(size as _);
        Self {
            inner: string,
            capa: Cell::new(size),
            _lifetime: PhantomData,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &'a mut [u8] {
        let raw_value = self.inner.as_raw();

        unsafe {
            std::slice::from_raw_parts_mut(RSTRING_PTR(raw_value) as *mut u8, self.capa.get())
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capa.get()
    }

    pub(crate) fn expand(&self, size: usize) {
        if size <= self.capacity() {
            return;
        }

        unsafe { rb_str_modify_expand(self.inner.as_raw(), size as _) };
    }

    pub(crate) fn set_len(&self, len: usize) {
        unsafe { rb_str_set_len(self.inner.as_raw(), len as _) }
    }

    pub(crate) fn into_inner(self) -> RString {
        self.inner
    }
}

impl Drop for RStringMut<'_> {
    fn drop(&mut self) {
        // Keep the value alive while we hold this RString
        let _ = rb_gc_guard!(self.inner.as_raw());
    }
}

pub(crate) fn nogvl_if_large<F, R>(input_len: usize, mut func: F) -> Result<R, Error>
where
    F: FnMut() -> R,
    R: Sized,
{
    const MAX_INPUT_LEN: usize = 8192;

    if input_len > MAX_INPUT_LEN {
        nogvl(func)
    } else {
        Ok(func())
    }
}

// https://docs.rs/rb-sys/0.9.97/rb_sys/bindings/uncategorized/fn.rb_thread_call_without_gvl.html
pub(crate) fn nogvl<F, R>(mut func: F) -> Result<R, Error>
where
    F: FnMut() -> R,
    R: Sized,
{
    unsafe extern "C" fn call_without_gvl<F, R>(arg: *mut c_void) -> *mut c_void
    where
        F: FnMut() -> R,
        R: Sized,
    {
        let arg = arg as *mut (&mut F, &mut MaybeUninit<R>);
        // Safety: we know that the pointer is valid since we initialized it in nogvl
        let (func, result) = unsafe { &mut *arg };
        result.write(func());

        null_mut()
    }

    let mut result = MaybeUninit::uninit();
    let mut args = (&mut func, &mut result);
    let arg_ptr = &mut args as *mut _ as *mut c_void;

    protect(|| unsafe {
        // https://github.com/ruby/ruby/blob/ruby_3_3/thread.c#L1550-L1634
        rb_thread_call_without_gvl(Some(call_without_gvl::<F, R>), arg_ptr, None, null_mut());
        rb_sys::Qnil as _
    })?;

    // The callback completed unless Ruby raised, in which case protect returned above.
    Ok(unsafe { result.assume_init() })
}
