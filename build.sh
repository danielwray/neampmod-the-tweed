#!/bin/bash

PLUGIN_NAME="the-tweed"
VST3_DIR="${HOME}/.vst3"
CLAP_DIR="${HOME}/.clap"

# Build the plugin bundles
cargo run --release -p xtask -- bundle ${PLUGIN_NAME} --release

# Install the plugin to ~/.vst3 and ~/.clap
if [ -d "$VST3_DIR" ]; then
  cp -r ./target/bundled/${PLUGIN_NAME}.vst3/Contents/x86_64-linux/${PLUGIN_NAME}.so "$VST3_DIR/${PLUGIN_NAME}-single.vst3"
  echo "Installed VST3 plugin ${PLUGIN_NAME} to $VST3_DIR"
fi

if [ -d "$CLAP_DIR" ]; then
  cp ./target/bundled/${PLUGIN_NAME}.clap "$CLAP_DIR/"
  echo "Installed CLAP plugin ${PLUGIN_NAME} to $CLAP_DIR"
fi

echo "Build complete. Plugin artifacts in ./target/bundled/"
echo "  - ${PLUGIN_NAME}-single.vst3"
echo "  - ${PLUGIN_NAME}.clap"
