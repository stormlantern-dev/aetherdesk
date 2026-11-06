//! Spike (#72): type at the macOS login window through Screen Sharing.
//! See README.md.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use md5::{Digest, Md5};
use num_bigint::BigUint;
use rand::RngCore;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const APPLE_DH: u8 = 30;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut host = "127.0.0.1".to_string();
    let mut port = 5900u16;
    let mut click: Option<(u16, u16)> = None;
    let mut cmd = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it.next().ok_or("--host needs a value")?.clone(),
            "--port" => port = it.next().ok_or("--port needs a value")?.parse()?,
            "--click" => {
                let v = it.next().ok_or("--click needs X,Y")?;
                let (x, y) = v.split_once(',').ok_or("--click needs X,Y")?;
                click = Some((x.parse()?, y.parse()?));
            }
            c => cmd = Some(c.to_string()),
        }
    }
    let mut s = TcpStream::connect((host.as_str(), port))?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let types = handshake(&mut s)?;
    match cmd.as_deref() {
        Some("probe") => {
            println!("security types: {}", types.iter().map(|t| format!("{t} ({})", name(*t))).collect::<Vec<_>>().join(", "));
            Ok(())
        }
        Some("login") => login(&mut s, &types, click),
        _ => Err("usage: rfb-login [--host H] [--port P] probe | login [--click X,Y]".into()),
    }
}

fn name(t: u8) -> &'static str {
    match t {
        1 => "none",
        2 => "VNC password",
        30 => "Apple DH",
        33 => "Apple RSA/SRP",
        35 => "Kerberos",
        36 => "Apple SRP",
        _ => "?",
    }
}

fn read_n(s: &mut TcpStream, n: usize) -> Res<Vec<u8>> {
    let mut b = vec![0; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

fn read_u16(s: &mut TcpStream) -> Res<u16> {
    let b = read_n(s, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn read_u32(s: &mut TcpStream) -> Res<u32> {
    let b = read_n(s, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Version exchange; returns the offered security types.
fn handshake(s: &mut TcpStream) -> Res<Vec<u8>> {
    let v = read_n(s, 12)?;
    let version = String::from_utf8_lossy(&v);
    println!("server version: {}", version.trim_end());
    // Echo the server's version (Apple offers 003.889, a 3.8 variant).
    s.write_all(&v)?;
    let n = read_n(s, 1)?[0] as usize;
    if n == 0 {
        let len = read_u32(s)? as usize;
        return Err(format!("server refused: {}", String::from_utf8_lossy(&read_n(s, len)?)).into());
    }
    Ok(read_n(s, n)?)
}

fn be_padded(x: &BigUint, len: usize) -> Vec<u8> {
    let b = x.to_bytes_be();
    let mut out = vec![0; len.saturating_sub(b.len())];
    out.extend_from_slice(&b);
    out
}

fn apple_dh(s: &mut TcpStream, user: &str, pass: &str) -> Res<()> {
    s.write_all(&[APPLE_DH])?;
    let g = BigUint::from_bytes_be(&read_n(s, 2)?);
    let key_len = read_u16(s)? as usize;
    let p = BigUint::from_bytes_be(&read_n(s, key_len)?);
    let server_pub = BigUint::from_bytes_be(&read_n(s, key_len)?);
    println!("Apple DH: generator {g}, key length {} bits", key_len * 8);

    let mut rng = rand::thread_rng();
    let mut priv_bytes = vec![0u8; key_len];
    rng.fill_bytes(&mut priv_bytes);
    let private = BigUint::from_bytes_be(&priv_bytes);
    let client_pub = g.modpow(&private, &p);
    let shared = server_pub.modpow(&private, &p);
    let key = Md5::digest(be_padded(&shared, key_len));

    // 64 bytes user, 64 bytes password, each NUL-terminated, rest random.
    let mut creds = [0u8; 128];
    rng.fill_bytes(&mut creds);
    for (off, field) in [(0, user), (64, pass)] {
        let b = field.as_bytes();
        if b.len() > 63 {
            return Err("user name or password longer than 63 bytes".into());
        }
        creds[off..off + b.len()].copy_from_slice(b);
        creds[off + b.len()] = 0;
    }
    let cipher = Aes128::new(GenericArray::from_slice(&key));
    for block in creds.chunks_mut(16) {
        cipher.encrypt_block(GenericArray::from_mut_slice(block));
    }
    s.write_all(&creds)?;
    s.write_all(&be_padded(&client_pub, key_len))?;
    creds.fill(0);

    match read_u32(s)? {
        0 => Ok(()),
        _ => {
            let reason = read_u32(s)
                .ok()
                .and_then(|len| read_n(s, len as usize).ok())
                .map(|r| String::from_utf8_lossy(&r).into_owned())
                .unwrap_or_default();
            Err(format!("authentication failed: {reason}").into())
        }
    }
}

fn key(s: &mut TcpStream, sym: u32) -> Res<()> {
    for down in [1u8, 0] {
        let mut m = vec![4, down, 0, 0];
        m.extend_from_slice(&sym.to_be_bytes());
        s.write_all(&m)?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn pointer(s: &mut TcpStream, mask: u8, x: u16, y: u16) -> Res<()> {
    let mut m = vec![5, mask];
    m.extend_from_slice(&x.to_be_bytes());
    m.extend_from_slice(&y.to_be_bytes());
    s.write_all(&m)?;
    Ok(())
}

fn login(s: &mut TcpStream, types: &[u8], click: Option<(u16, u16)>) -> Res<()> {
    if !types.contains(&APPLE_DH) {
        return Err("server does not offer Apple DH (30)".into());
    }
    print!("Mac account: ");
    io::stdout().flush()?;
    let mut user = String::new();
    io::stdin().read_line(&mut user)?;
    let pass = rpassword::prompt_password("Password (for Screen Sharing): ")?;
    apple_dh(s, user.trim(), &pass)?;
    drop(pass);
    println!("authenticated");

    s.write_all(&[1])?; // ClientInit, shared
    let w = read_u16(s)?;
    let h = read_u16(s)?;
    let _pixel_format = read_n(s, 16)?;
    let len = read_u32(s)? as usize;
    let _name = read_n(s, len)?; // the computer name; not printed
    println!("framebuffer {w}x{h}");

    // Drain whatever the server sends so it never blocks on us.
    let mut r = s.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while matches!(r.read(&mut buf), Ok(n) if n > 0) {}
    });

    if let Some((x, y)) = click {
        pointer(s, 0, x, y)?;
        std::thread::sleep(Duration::from_millis(100));
        pointer(s, 1, x, y)?;
        std::thread::sleep(Duration::from_millis(50));
        pointer(s, 0, x, y)?;
        println!("clicked at {x},{y}");
        std::thread::sleep(Duration::from_millis(500));
    }

    let text = rpassword::prompt_password("Text to type at the Mac (not echoed, Return is added): ")?;
    for c in text.chars() {
        // X11 keysyms: Latin-1 maps directly, other Unicode is 0x0100_0000 + code point.
        let cp = c as u32;
        let sym = if (0x20..=0xff).contains(&cp) { cp } else { 0x0100_0000 + cp };
        key(s, sym)?;
    }
    drop(text);
    key(s, 0xff0d)?; // Return
    println!("typed the text and Return; holding the connection for 10 s");
    std::thread::sleep(Duration::from_secs(10));
    Ok(())
}
