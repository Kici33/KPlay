//! `--demo`: feeds an animated test pattern into the window so the controls
//! (scroll to resize, fullscreen, indicator) can be tried without a phone.

use crate::display::{Frame, UiLink};
use std::{thread, time::Duration};

pub fn spawn(ui: UiLink) {
    thread::spawn(move || {
        ui.connected("Demo".into());
        let (w, h) = (886u32, 1920u32);
        let mut t = 0u32;
        loop {
            let mut px = vec![0u32; (w * h) as usize];
            let bar = (t * 9) % h;
            for y in 0..h {
                for x in 0..w {
                    let grid = x % 100 == 0 || y % 100 == 0;
                    let mut c = if grid {
                        0x00FF_FFFF
                    } else {
                        ((x * 255 / w) << 16) | ((y * 255 / h) << 8) | ((t * 3) % 256)
                    };
                    if y.abs_diff(bar) < 6 {
                        c = 0x00FF_FFFF;
                    }
                    px[(y * w + x) as usize] = c;
                }
            }
            ui.push_frame(Frame { w, h, px });
            t += 1;
            thread::sleep(Duration::from_millis(33));
        }
    });
}
