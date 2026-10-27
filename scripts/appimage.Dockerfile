FROM ubuntu:24.04@sha256:24737cef17f4e22b1977df7d9a05e77c4e28b7da5b8911f8d8ccda136bf3573c
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get -o Acquire::ForceIPv4=true update && apt-get -o Acquire::ForceIPv4=true install -y --no-install-recommends \
    build-essential ninja-build pkg-config python3-venv python3-jinja2 python3-packaging \
    curl ca-certificates xz-utils patchelf file desktop-file-utils \
    libffi-dev libpcre2-dev zlib1g-dev libmount-dev libselinux1-dev \
    libfontconfig-dev libfreetype-dev libpng-dev libpixman-1-dev \
    libxcb-render0-dev libxcb-shm0-dev libx11-dev libxext-dev libxrender-dev \
    libfribidi-dev libgraphene-1.0-dev libepoxy-dev libxkbcommon-dev \
    libxrandr-dev libxi-dev libxcursor-dev libxdamage-dev libxfixes-dev libxinerama-dev \
    libgdk-pixbuf-2.0-dev libdbus-1-dev libjson-glib-dev libappstream-dev \
    libxml2-dev libexpat1-dev fonts-dejavu-core xvfb xauth dbus-daemon \
    && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv --system-site-packages /opt/build-tools && /opt/build-tools/bin/pip install --no-cache-dir meson==1.8.5
RUN apt-get -o Acquire::ForceIPv4=true update && apt-get -o Acquire::ForceIPv4=true install -y --no-install-recommends \
    gettext librsvg2-common libxkbcommon-x11-dev libegl-dev libgles-dev libdrm-dev sassc \
    && rm -rf /var/lib/apt/lists/*
ENV PATH=/opt/build-tools/bin:$PATH
WORKDIR /work
