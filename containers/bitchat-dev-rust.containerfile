# bitchat-rs build image: Fedora at the version the release targets. That is
# a hard pin, not a rolling one: flatpak apps run against their runtime's
# glibc (freedesktop 25.08 carries 2.42, the Fedora 43 set), so a binary
# built on a newer Fedora dies with "GLIBC_2.4x not found" inside the
# sandbox. Same reasoning as orchard's own containerfile. Plus gcc for the C
# glue some -sys crates generate, and the gpui native stack: fontconfig and
# freetype (text), xkbcommon (input), clang-devel (for bindgen), and
# pkg-config so those builds find their libraries.
FROM fedora:43

# Rust toolchain baked into the image: CARGO_HOME/RUSTUP_HOME live in image
# layers (not volumes), so the image is self-contained; only target/ is
# shared with the host checkout.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/bin:/bin
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal

RUN dnf -y install gcc pkgconf-pkg-config fontconfig-devel freetype-devel \
      libxkbcommon-devel libxkbcommon-x11-devel clang-devel \
      flatpak python3 \
      # libdbus headers for bluer (BlueZ over D-Bus).
      dbus-devel \
      # Vulkan loader + lavapipe (software rendering) so gpui can run in
      # the container without GPU device passthrough.
      mesa-vulkan-drivers vulkan-loader \
  && dnf clean all
