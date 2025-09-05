#!/bin/bash
# Example install script demonstrating the use of AVOCADO_BUILD_EXT_SYSROOT
# This script runs after SDK compilation and installs compiled code into the extension

set -e

echo "Installing compiled application into extension..."

# The AVOCADO_BUILD_EXT_SYSROOT environment variable is automatically set
# to the fully expanded path of the extension sysroot that has this dependency
echo "Extension sysroot: $AVOCADO_BUILD_EXT_SYSROOT"

# Example: Get application name from environment or use default
APP_NAME=${APP_NAME:-"my-app"}
INSTALL_PATH=${INSTALL_PATH:-"/opt/$APP_NAME"}

# Check if compiled artifacts exist
COMPILED_DIR="$AVOCADO_SDK_PREFIX/compiled/$APP_NAME"
if [ ! -d "$COMPILED_DIR" ]; then
    echo "Error: Compiled application not found at $COMPILED_DIR"
    exit 1
fi

# Create installation directory in the extension sysroot
FULL_INSTALL_PATH="$AVOCADO_BUILD_EXT_SYSROOT$INSTALL_PATH"
mkdir -p "$FULL_INSTALL_PATH"

# Copy compiled artifacts to the extension sysroot
echo "Copying compiled artifacts to $FULL_INSTALL_PATH"
cp -r "$COMPILED_DIR"/* "$FULL_INSTALL_PATH/"

# Make binaries executable
find "$FULL_INSTALL_PATH" -type f -name "$APP_NAME" -exec chmod +x {} \; 2>/dev/null || true

# Create systemd service file in the extension sysroot
mkdir -p "$AVOCADO_BUILD_EXT_SYSROOT/usr/lib/systemd/system"
cat > "$AVOCADO_BUILD_EXT_SYSROOT/usr/lib/systemd/system/$APP_NAME.service" << EOF
[Unit]
Description=$APP_NAME Application
After=network.target

[Service]
Type=exec
ExecStart=$INSTALL_PATH/$APP_NAME
Restart=always
RestartSec=5
User=$APP_NAME
Group=$APP_NAME
WorkingDirectory=$INSTALL_PATH

[Install]
WantedBy=multi-user.target
EOF

# Create system user via sysusers.d in the extension sysroot
mkdir -p "$AVOCADO_BUILD_EXT_SYSROOT/usr/lib/sysusers.d"
cat > "$AVOCADO_BUILD_EXT_SYSROOT/usr/lib/sysusers.d/$APP_NAME.conf" << EOF
u $APP_NAME - "$APP_NAME Application User" /var/lib/$APP_NAME /bin/false
EOF

# Create application directories in the extension sysroot
mkdir -p "$AVOCADO_BUILD_EXT_SYSROOT/var/lib/$APP_NAME"
mkdir -p "$AVOCADO_BUILD_EXT_SYSROOT/var/log/$APP_NAME"

echo "Installation completed successfully!"
echo "Application installed to: $INSTALL_PATH"
echo "Service file: /usr/lib/systemd/system/$APP_NAME.service"
echo "System user: $APP_NAME"
