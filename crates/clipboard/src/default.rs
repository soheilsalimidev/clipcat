use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use arboard::{ClearExtLinux, GetExtLinux, SetExtLinux};
use bytes::Bytes;
use clipcat_base::ClipboardContent;

use crate::{
    listener::{WaylandListener, X11Listener},
    traits::EventObserver,
    ClipboardKind, ClipboardLoad, ClipboardStore, ClipboardSubscribe, Error, Subscriber,
};

#[derive(Clone)]
pub struct Clipboard {
    listener: Arc<dyn ClipboardSubscribe<Subscriber = Subscriber>>,
    clipboard_kind: arboard::LinuxClipboardKind,
    clear_on_drop: Arc<AtomicBool>,
}

impl Clipboard {
    /// # Errors
    pub fn new(
        clipboard_kind: ClipboardKind,
        event_observers: Vec<Arc<dyn EventObserver>>,
    ) -> Result<Self, Error> {
        let listener: Arc<dyn ClipboardSubscribe<Subscriber = Subscriber>> =
            if let Ok(display_name) = std::env::var("WAYLAND_DISPLAY") {
                tracing::info!(
                    "Build Wayland listener ({clipboard_kind}) with display `{display_name}`"
                );
                Arc::new(WaylandListener::new(clipboard_kind)?)
            } else {
                match std::env::var("DISPLAY") {
                    Ok(display_name) => {
                        tracing::info!(
                            "Build X11 listener ({clipboard_kind}) with display `{display_name}`"
                        );
                        Arc::new(X11Listener::new(
                            Some(display_name),
                            clipboard_kind,
                            event_observers,
                        )?)
                    }
                    Err(_) => Arc::new(X11Listener::new(None, clipboard_kind, event_observers)?),
                }
            };

        let clear_on_drop = Arc::new(AtomicBool::from(false));
        let clipboard_kind = match clipboard_kind {
            ClipboardKind::Clipboard => arboard::LinuxClipboardKind::Clipboard,
            ClipboardKind::Primary => arboard::LinuxClipboardKind::Primary,
            ClipboardKind::Secondary => arboard::LinuxClipboardKind::Secondary,
        };
        Ok(Self { listener, clipboard_kind, clear_on_drop })
    }
}

impl ClipboardSubscribe for Clipboard {
    type Subscriber = Subscriber;

    fn subscribe(&self) -> Result<Self::Subscriber, Error> { self.listener.subscribe() }
}

impl ClipboardLoad for Clipboard {
    fn load(&self, mime: Option<mime::Mime>) -> Result<ClipboardContent, Error> {
        match mime {
            None => self
                .load(Some(mime::TEXT_PLAIN_UTF_8))
                .map_or_else(|_| self.load(Some(mime::IMAGE_PNG)), Ok),
            Some(mime) => {
                let mut arboard = arboard::Clipboard::new()?;

                if mime.type_() == mime::TEXT {
                    match arboard.get().clipboard(self.clipboard_kind).text() {
                        Ok(text) => Ok(ClipboardContent::Plaintext(text)),
                        Err(arboard::Error::ClipboardNotSupported) => unreachable!(),
                        Err(err) => {
                            tracing::warn!("{err}");
                            Err(Error::Empty)
                        }
                    }
                } else if mime.type_() == mime::IMAGE {
                    match arboard.get().clipboard(self.clipboard_kind).image() {
                        Ok(arboard::ImageData { width, height, bytes }) => {
                            Ok(ClipboardContent::Image {
                                width,
                                height,
                                bytes: Bytes::from(bytes.into_owned()),
                            })
                        }
                        Err(arboard::Error::ClipboardNotSupported) => unreachable!(),
                        Err(err) => {
                            tracing::warn!("{err}");
                            Err(Error::Empty)
                        }
                    }
                } else {
                    Err(Error::Empty)
                }
            }
        }
    }
}

impl ClipboardStore for Clipboard {
    #[inline]
    fn store(&self, content: ClipboardContent) -> Result<(), Error> {
        let mut arboard = arboard::Clipboard::new()?;
        let clipboard_kind = self.clipboard_kind;
        let clear_on_drop = self.clear_on_drop.clone();

        let _join_handle = std::thread::spawn(move || {
            clear_on_drop.store(true, Ordering::Relaxed);

            let _result = match content {
                ClipboardContent::Plaintext(text) => {
                    arboard.set().clipboard(clipboard_kind).wait().text(text)
                }
                ClipboardContent::Image { width, height, bytes } => arboard
                    .set()
                    .clipboard(clipboard_kind)
                    .wait()
                    .image(arboard::ImageData { width, height, bytes: bytes.to_vec().into() }),
            };

            clear_on_drop.store(false, Ordering::Relaxed);
        });
        Ok(())
    }

    #[inline]
    fn clear(&self) -> Result<(), Error> {
        arboard::Clipboard::new()?.clear_with().clipboard(self.clipboard_kind)?;
        self.clear_on_drop.store(false, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if self.clear_on_drop.load(Ordering::Relaxed) {
            drop(self.clear());
        }
    }
}
