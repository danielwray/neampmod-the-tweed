FROM rust:1.92

RUN apt-get update && apt-get install -y \
    build-essential \
    libasound2-dev \
    libxcb-shape0-dev \
    libxcb-xfixes0-dev \
    libgl1-mesa-dev \
    libx11-xcb-dev \
    libx11-dev \
    libgtk-3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
