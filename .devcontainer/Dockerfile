# Linux development image for AetherDesk (Hyprland client + shared crates).
# Arch-based to match the CachyOS host: same SDL3 / ffmpeg / Wayland versions.
FROM docker.io/library/archlinux:base-devel

RUN pacman -Syu --noconfirm --needed \
        git openssh rustup clang llvm mold pkgconf cmake \
        sdl3 ffmpeg libva \
        wayland wayland-protocols libxkbcommon libglvnd mesa vulkan-icd-loader \
        wayland-utils mesa-utils \
    && pacman -Scc --noconfirm

# Same UID/GID as the host user, so files in the bind-mounted workspace keep
# their ownership (podman runs with --userns=keep-id).
ARG USERNAME=dev
ARG USER_UID=1000
ARG USER_GID=1000
RUN groupadd --gid ${USER_GID} ${USERNAME} \
    && useradd --uid ${USER_UID} --gid ${USER_GID} --create-home --shell /bin/bash ${USERNAME} \
    && echo "${USERNAME} ALL=(root) NOPASSWD:ALL" > /etc/sudoers.d/${USERNAME} \
    && mkdir -p /run/user/${USER_UID} \
    && chown ${USER_UID}:${USER_GID} /run/user/${USER_UID} \
    && chmod 700 /run/user/${USER_UID}

# Lets C build scripts (ring) compile for aarch64-apple-darwin, so the macOS
# host can be type-checked here with `cargo check/clippy --target
# aarch64-apple-darwin`. Check-only: nothing built this way gets linked.
COPY macos-check /opt/macos-check
ENV CC_aarch64_apple_darwin=clang \
    AR_aarch64_apple_darwin=llvm-ar \
    CFLAGS_aarch64_apple_darwin="-ffreestanding -nostdlibinc -I/opt/macos-check"

USER ${USERNAME}
# The exact version comes from rust-toolchain.toml; rustup installs it on first use.
RUN rustup default stable \
    && rustup component add rustfmt clippy rust-analyzer \
    && rustup target add aarch64-apple-darwin

ENV XDG_RUNTIME_DIR=/run/user/${USER_UID}
