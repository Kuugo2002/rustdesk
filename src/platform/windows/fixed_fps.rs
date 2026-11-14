use crate::server::video_service::VIDEO_QOS;
use scrap::{Frame, PixelBuffer, Pixfmt, TraitCapturer, TraitPixelBuffer};
use std::{io, time::Duration};

pub(crate) struct Capturer {
    inner: Box<dyn TraitCapturer>,
    cached: Option<CachedFrame>,
}

enum CachedFrame {
    Pixels(Vec<u8>, Pixfmt, usize, usize),
    #[cfg(feature = "vram")]
    Texture(Texture, usize),
}

impl CachedFrame {
    fn capture(frame: &Frame<'_>, previous: Option<Self>) -> io::Result<Self> {
        match frame {
            Frame::PixelBuffer(pixels) => {
                let mut data = match previous {
                    Some(Self::Pixels(data, ..)) => data,
                    _ => Vec::new(),
                };
                data.resize(pixels.data().len(), 0);
                data.copy_from_slice(pixels.data());
                Ok(Self::Pixels(
                    data,
                    pixels.pixfmt(),
                    pixels.width(),
                    pixels.height(),
                ))
            }
            #[cfg(feature = "vram")]
            Frame::Texture((texture, rotation)) => {
                let previous = match previous {
                    Some(Self::Texture(texture, _)) => Some(texture),
                    _ => None,
                };
                Ok(Self::Texture(Texture::copy(*texture, previous)?, *rotation))
            }
            #[cfg(not(feature = "vram"))]
            Frame::Texture(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Texture capture requires vram",
            )),
        }
    }

    fn frame(&self) -> Frame<'_> {
        match self {
            Self::Pixels(data, format, width, height) => {
                Frame::PixelBuffer(PixelBuffer::new(data, *format, *width, *height))
            }
            #[cfg(feature = "vram")]
            Self::Texture(texture, rotation) => Frame::Texture((texture.0 .0.cast(), *rotation)),
        }
    }
}

impl Capturer {
    pub(crate) fn new(inner: Box<dyn TraitCapturer>) -> Self {
        Self {
            inner,
            cached: None,
        }
    }

    fn frame_with_mode(&mut self, timeout: Duration, fixed: bool) -> io::Result<Frame<'_>> {
        if !fixed {
            self.cached = None;
            return self.inner.frame(timeout);
        }
        // DXGI must not spend another frame interval waiting for desktop changes. The
        // portable capturer reports is_gdi() and uses this argument to pace its helper.
        let timeout = if self.inner.is_gdi() {
            timeout
        } else {
            Duration::ZERO
        };
        match self.inner.frame(timeout) {
            Ok(frame) if frame.valid() => {
                self.cached = Some(CachedFrame::capture(&frame, self.cached.take())?);
            }
            Ok(_) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(err) => {
                self.cached = None;
                return Err(err);
            }
        }
        self.cached
            .as_ref()
            .map(CachedFrame::frame)
            .ok_or_else(|| io::ErrorKind::WouldBlock.into())
    }
}

impl TraitCapturer for Capturer {
    fn frame(&mut self, timeout: Duration) -> io::Result<Frame<'_>> {
        let fixed = VIDEO_QOS.lock().unwrap().fixed_60_fps();
        self.frame_with_mode(timeout, fixed)
    }

    fn is_gdi(&self) -> bool {
        self.inner.is_gdi()
    }

    fn set_gdi(&mut self) -> bool {
        self.cached = None;
        self.inner.set_gdi()
    }

    #[cfg(feature = "vram")]
    fn device(&self) -> scrap::AdapterDevice {
        self.inner.device()
    }

    #[cfg(feature = "vram")]
    fn set_output_texture(&mut self, texture: bool) {
        self.cached = None;
        self.inner.set_output_texture(texture);
    }
}

#[cfg(feature = "vram")]
struct Com<T>(*mut T);

#[cfg(feature = "vram")]
impl<T> Drop for Com<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (*(self.0 as *mut winapi::um::unknwnbase::IUnknown)).Release() };
        }
    }
}

#[cfg(feature = "vram")]
struct Texture(Com<winapi::um::d3d11::ID3D11Texture2D>);

#[cfg(feature = "vram")]
impl Texture {
    fn copy(source: *mut std::ffi::c_void, previous: Option<Self>) -> io::Result<Self> {
        use std::ptr;
        use winapi::um::d3d11::*;

        // Duplication owns the source; retaining its COM pointer does not preserve its pixels
        // after ReleaseFrame. Keep our own resource for frames sent while the desktop is idle.
        unsafe {
            let source = source as *mut ID3D11Texture2D;
            let mut device = Com(ptr::null_mut());
            (*source).GetDevice(&mut device.0);
            let mut desc = std::mem::zeroed();
            (*source).GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED;
            let previous = previous.filter(|texture| {
                let mut old: D3D11_TEXTURE2D_DESC = std::mem::zeroed();
                (*texture.0 .0).GetDesc(&mut old);
                old.Width == desc.Width && old.Height == desc.Height && old.Format == desc.Format
            });
            let texture = match previous {
                Some(texture) => texture.0,
                None => {
                    let mut texture = Com(ptr::null_mut());
                    let result = (*device.0).CreateTexture2D(&desc, ptr::null(), &mut texture.0);
                    if result < 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::Other,
                            format!("Failed to cache desktop texture: {result:#x}"),
                        ));
                    }
                    texture
                }
            };
            let mut context = Com(ptr::null_mut());
            (*device.0).GetImmediateContext(&mut context.0);
            (*context.0).CopyResource(texture.0.cast(), source.cast());
            Ok(Self(texture))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StaticScreen(bool);

    impl TraitCapturer for StaticScreen {
        fn frame(&mut self, timeout: Duration) -> io::Result<Frame<'_>> {
            assert_eq!(timeout, Duration::ZERO);
            if self.0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.0 = true;
            Ok(Frame::PixelBuffer(PixelBuffer::with_BGRA(
                &[1, 2, 3, 4],
                1,
                1,
            )))
        }

        fn is_gdi(&self) -> bool {
            false
        }
        fn set_gdi(&mut self) -> bool {
            false
        }
        #[cfg(feature = "vram")]
        fn device(&self) -> scrap::AdapterDevice {
            Default::default()
        }
        #[cfg(feature = "vram")]
        fn set_output_texture(&mut self, _: bool) {}
    }

    #[test]
    fn static_desktop_continues_past_the_old_repeat_limit() {
        let mut capturer = Capturer::new(Box::new(StaticScreen(false)));
        for _ in 0..120 {
            let Frame::PixelBuffer(frame) = capturer.frame_with_mode(Duration::ZERO, true).unwrap()
            else {
                panic!("Expected desktop pixels");
            };
            assert_eq!(frame.data(), &[1, 2, 3, 4]);
        }
    }

    #[test]
    fn other_frame_rates_do_not_repeat_static_frames() {
        let mut capturer = Capturer::new(Box::new(StaticScreen(false)));
        assert!(capturer.frame_with_mode(Duration::ZERO, true).is_ok());
        assert!(
            matches!(capturer.frame_with_mode(Duration::ZERO, false), Err(err) if err.kind() == io::ErrorKind::WouldBlock)
        );
        assert!(
            matches!(capturer.frame_with_mode(Duration::ZERO, true), Err(err) if err.kind() == io::ErrorKind::WouldBlock)
        );
    }
}
