# Build container for flip-companion
# Forces x86_64 so Mac ARM builds produce device-compatible binaries
FROM --platform=linux/amd64 registry.fedoraproject.org/fedora:43

ARG USERNAME=vscode
ARG USER_UID=1000
ARG USER_GID=${USER_UID}

# Create non-root user with passwordless sudo
RUN groupadd --gid ${USER_GID} ${USERNAME} \
    && useradd --uid ${USER_UID} --gid ${USER_GID} -m -s /bin/bash ${USERNAME} \
    && dnf install -y sudo \
    && echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/${USERNAME}

# Build dependencies only
# Runtime deps (kscreen, ydotool) live on the target device, not here
RUN dnf install -y \
    gcc \
    gcc-c++ \
    cmake \
    pkg-config \
    # Slint rendering (femtovg / winit backend)
    wayland-devel \
    libxkbcommon-devel \
    mesa-libEGL-devel \
    mesa-libGL-devel \
    fontconfig-devel \
    # D-Bus client (zbus build)
    dbus-devel \
    # Dev tools
    git \
    just \
    && dnf clean all

# Rust toolchain — installed system-wide so any user can access it
ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH=/usr/local/cargo/bin:${PATH}

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
    --default-toolchain stable \
    --component clippy rustfmt \
    && chmod -R a+rw ${RUSTUP_HOME} ${CARGO_HOME}

USER ${USERNAME}
