/* Minimal stand-in for Apple's TargetConditionals.h, so C build scripts (ring)
 * can compile for aarch64-apple-darwin in the Linux devcontainer. Only good
 * for `cargo check`/`clippy`; nothing built this way is ever linked or run. */
#pragma once
#define TARGET_OS_MAC 1
#define TARGET_OS_OSX 1
#define TARGET_OS_IPHONE 0
#define TARGET_OS_IOS 0
#define TARGET_OS_WATCH 0
#define TARGET_OS_TV 0
#define TARGET_OS_SIMULATOR 0
#define TARGET_CPU_ARM64 1
#define TARGET_CPU_X86_64 0
