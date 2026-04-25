// User identity widget for the navbar.
//
// Fetches `GET /api/me` once on mount and renders the username + avatar +
// logout link. Designed to live inside the existing `<nav class="navbar">`
// blocks at the right-hand edge.
//
// On 401 the underlying API helper triggers a redirect to `/auth/login`, so
// this component never renders an "unauthenticated" state — by the time it
// mounts, the request either succeeds or the page navigates away.

use leptos::prelude::*;

#[component]
pub fn UserBadge() -> impl IntoView {
    // `LocalResource` runs the async fetch on the WASM event loop and gives us
    // a reactive `Option<…>`. We collapse the error into `Option<UserInfo>`
    // here because `gloo_net::Error` is not `Clone` (required by Resource read
    // semantics) and the only failure mode worth distinguishing — 401 — is
    // already handled by `check_auth` redirecting the page away.
    let me = LocalResource::new(|| async { crate::api::fetch_me().await.ok() });

    view! {
        <span class="navbar-user">
            <Suspense fallback=|| view! { <span class="user-loading">"…"</span> }>
                {move || me.get().map(|opt| match opt.take() {
                    Some(user) => view! { <UserDetails user=user /> }.into_any(),
                    None => view! { <span /> }.into_any(),
                })}
            </Suspense>
        </span>
    }
}

/// Renders the avatar + name + logout link for a successfully-fetched user.
/// Split out into its own component so the gravatar URL computation lives
/// next to the markup that uses it.
#[component]
fn UserDetails(user: shared::UserInfo) -> impl IntoView {
    let avatar_url = avatar_url_for(&user);
    let name = user.name.clone();
    view! {
        <img class="user-avatar" src=avatar_url alt=name.clone() />
        <span class="user-name">{name}</span>
        // Plain anchor so the browser does a real navigation — the backend
        // /auth/logout route clears the cookie and bounces to the IdP's
        // end-session endpoint, which only works as a top-level navigation.
        <a class="user-logout" href="/auth/logout" title="Sign out">"sign out"</a>
    }
}

/// Pick an avatar URL: prefer the IdP-provided `picture` claim, fall back to
/// Gravatar's identicon endpoint keyed by the lowercase email's MD5 hash.
/// Returns a stable transparent-pixel data URI when the user has neither —
/// rare but possible for service accounts viewed in the navbar context.
fn avatar_url_for(user: &shared::UserInfo) -> String {
    if let Some(pic) = user.picture.as_ref().filter(|s| !s.is_empty()) {
        return pic.clone();
    }
    if let Some(email) = user.email.as_ref().filter(|s| !s.is_empty()) {
        let hash = md5_hex(email.trim().to_lowercase().as_bytes());
        // d=identicon: deterministic geometric pattern keyed on the hash.
        // s=64: 64x64 px, plenty for a 24px navbar avatar at 2x DPI.
        return format!("https://www.gravatar.com/avatar/{hash}?d=identicon&s=64");
    }
    // Stable 1x1 transparent PNG.
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII="
        .to_string()
}

// ── MD5 (used only for Gravatar URL hashing) ──────────────────────────────────
//
// We need an MD5 specifically because Gravatar mandates it. This is a tiny
// dedicated implementation rather than pulling in a crate — saves a dep and
// the surface area is exactly one function called once per page load. Output
// is the lowercase hex digest as Gravatar expects.

fn md5_hex(input: &[u8]) -> String {
    let digest = md5(input);
    let mut s = String::with_capacity(32);
    for byte in digest.iter() {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

#[allow(clippy::many_single_char_names)]
fn md5(input: &[u8]) -> [u8; 16] {
    // Per-round shift amounts and constants from RFC 1321.
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    // Pad: append 0x80, then zero bytes, then the 64-bit little-endian bit length.
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0): (u32, u32, u32, u32) =
        (0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476);

    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, word) in chunk.chunks(4).enumerate() {
            m[i] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[i])
                    .wrapping_add(m[g])
                    .rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        // RFC 1321 test vectors.
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"message digest"),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
    }
}
