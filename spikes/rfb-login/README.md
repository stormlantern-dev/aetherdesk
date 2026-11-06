# Spike: login window input through Screen Sharing (#72)

macOS ignores synthetic input at the login window from third-party processes
(#3, #18), but accepts input sent through its own Screen Sharing service.
This spike checks whether a third-party RFB client can use that path.

Run it **on the Mac**, against `127.0.0.1` (the relay variant: the daemon would
talk to Screen Sharing on the Mac itself). Screen Sharing must be on
(System Settings → General → Sharing → Screen Sharing).

```zsh
cargo run --release -- probe
```
prints the protocol version and the security types offered.

```zsh
cargo run --release -- login [--click X,Y]
```
asks for a Mac account and password (not echoed), authenticates with Apple's
Diffie-Hellman type (30), optionally clicks at `X,Y` (framebuffer pixels), then
asks for text to type (not echoed) and types it followed by Return.

To test the login window: log out on the Mac, then run `login` over SSH.
