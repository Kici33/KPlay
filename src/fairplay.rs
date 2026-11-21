//! FairPlay (v3 / "PlayFair") bindings. The heavy lifting is the vendored C
//! implementation from UxPlay/RPiPlay in `vendor/`.

unsafe extern "C" {
    fn kp_fp_setup(req: *const u8, res: *mut u8) -> i32;
    fn kp_fp_handshake(req: *const u8, res: *mut u8) -> i32;
    fn kp_fp_decrypt(keymsg: *const u8, ekey: *const u8, out: *mut u8);
}

/// Phase 1: 16-byte request -> 142-byte reply.
pub fn setup(req: &[u8]) -> Option<[u8; 142]> {
    if req.len() != 16 {
        return None;
    }
    let mut res = [0u8; 142];
    (unsafe { kp_fp_setup(req.as_ptr(), res.as_mut_ptr()) } == 0).then_some(res)
}

/// Phase 2: 164-byte request -> 32-byte reply. The request must be kept: it
/// is the key material needed by [`decrypt_key`].
pub fn handshake(req: &[u8]) -> Option<[u8; 32]> {
    if req.len() != 164 {
        return None;
    }
    let mut res = [0u8; 32];
    (unsafe { kp_fp_handshake(req.as_ptr(), res.as_mut_ptr()) } == 0).then_some(res)
}

/// Decrypt the 72-byte `ekey` from SETUP into the 16-byte AES key.
pub fn decrypt_key(keymsg: &[u8; 164], ekey: &[u8]) -> Option<[u8; 16]> {
    if ekey.len() != 72 {
        return None;
    }
    let mut out = [0u8; 16];
    unsafe { kp_fp_decrypt(keymsg.as_ptr(), ekey.as_ptr(), out.as_mut_ptr()) };
    Some(out)
}
