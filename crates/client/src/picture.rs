//! Pointer shapes (and, later, window icons) from the agent's images.
//!
//! Windows builds both from a 32-bit colour bitmap with alpha plus a
//! monochrome mask; with an alpha bitmap the mask is ignored, so it is left
//! empty.

#![allow(unsafe_code)]

use ferostream_proto::image::Image;
use windows::Win32::Graphics::Gdi::{
    BI_BITFIELDS, BITMAPINFO, BITMAPV5HEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HCURSOR, HICON, ICONINFO};

/// A Windows pointer with the image and hot spot the agent sent.
pub fn cursor(hot_x: u16, hot_y: u16, image: &Image) -> Option<HCURSOR> {
    let hicon = build(image, false, u32::from(hot_x), u32::from(hot_y))?;
    Some(HCURSOR(hicon.0))
}

fn build(image: &Image, is_icon: bool, hot_x: u32, hot_y: u32) -> Option<HICON> {
    let px = image.decode().ok()?;
    let (w, h) = (i32::from(image.width), i32::from(image.height));
    let header = BITMAPV5HEADER {
        bV5Size: std::mem::size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: w,
        // Negative height: rows top to bottom, as the image has them.
        bV5Height: -h,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00ff_0000,
        bV5GreenMask: 0x0000_ff00,
        bV5BlueMask: 0x0000_00ff,
        bV5AlphaMask: 0xff00_0000,
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = unsafe {
        CreateDIBSection(
            None,
            (&raw const header).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
    }
    .ok()?;
    if bits.is_null() {
        delete(color);
        return None;
    }
    // SAFETY: the section is w * h 32-bit pixels, the size `px` has (the
    // image decoder checked it).
    unsafe {
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits.cast::<u8>(), px.len());
    }
    // A monochrome bitmap's rows are padded to 16 bits.
    let mask_bits = vec![0u8; (image.width.div_ceil(16) as usize) * 2 * image.height as usize];
    let mask = unsafe { CreateBitmap(w, h, 1, 1, Some(mask_bits.as_ptr().cast())) };
    let info = ICONINFO {
        fIcon: is_icon.into(),
        xHotspot: hot_x,
        yHotspot: hot_y,
        hbmMask: mask,
        hbmColor: color,
    };
    let made = unsafe { CreateIconIndirect(&info) }.ok();
    // The icon has its own copies of both bitmaps.
    delete(color);
    delete(mask);
    made
}

fn delete(b: HBITMAP) {
    unsafe {
        let _ = DeleteObject(b.into());
    }
}
