"""Shared container utility for SDK container operations."""

import os
import shlex
import subprocess
import sys
from typing import List, Dict, Optional, Union
from avocado.utils.output import print_error


class SdkContainer:
    def __init__(self, container_tool: str = "docker", verbose: bool = False):
        self.container_tool = container_tool
        self.cwd = os.getcwd()
        self.verbose = verbose

    def run_in_container(
        self,
        container_image: str,
        target: str,
        command: Optional[List[str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        env_vars: Optional[Dict[str, str]] = None,
        verbose: bool = False,
        source_environment: bool = True,
        use_entrypoint: bool = True,
        interactive: bool = False,
        repo_url: Optional[str] = None,
        repo_release: Optional[str] = None,
        container_args: Optional[List[str]] = None,
    ) -> bool:
        os.makedirs("_avocado", exist_ok=True)
        bash_cmd = ["bash", "-c"]
        cmd = ""

        if use_entrypoint:
            entrypoint_script = self._create_entrypoint_script(
                source_environment)
            cmd += f"{entrypoint_script}\n"

        if command and isinstance(command, list):
            cmd += f"{' '.join(command)}"
        elif command:
            # escaped_command = ' '.join(shlex.quote(arg) for arg in command)
            cmd += f"{command}"
        else:
            cmd += ""

        bash_cmd.append(cmd)

        # If verbose is specified for this call, override instance setting
        verbose_final = verbose or self.verbose

        try:
            # If a repo_url is provided, add it to the environment variables
            if repo_url:
                if env_vars is None:
                    env_vars = {}
                env_vars["AVOCADO_SDK_REPO_URL"] = repo_url

            # If a repo_url is provided, add it to the environment variables
            if repo_release:
                if env_vars is None:
                    env_vars = {}
                env_vars["AVOCADO_SDK_REPO_RELEASE"] = repo_release

            container_cmd = self._build_container_command(
                container_image=container_image,
                command=bash_cmd,
                target=target,
                env_vars=env_vars,
                container_name=container_name,
                detach=detach,
                rm=rm,
                interactive=interactive,
                container_args=container_args,
            )

            return self._execute_container_command(container_cmd, detach, verbose_final)

        except Exception as e:
            print_error(f"Container execution failed: {e}")
            return False

    def _build_container_command(
        self,
        container_image: str,
        command: Union[str, List[str]],
        target: Optional[str] = None,
        env_vars: Optional[Dict[str, str]] = None,
        container_name: Optional[str] = None,
        detach: bool = False,
        rm: bool = True,
        interactive: bool = False,
        container_args: Optional[List[str]] = None,
    ) -> List[str]:
        """Build the complete container command."""
        container_cmd = [self.container_tool, "run"]

        # Container options
        if rm:
            container_cmd.append("--rm")
        if container_name:
            container_cmd.extend(["--name", container_name])
        if detach:
            container_cmd.append("-d")

        if interactive:
            container_cmd.extend(["-i", "-t"])

        # Default volume mounts
        container_cmd.extend(
            [
                "-v",
                f"{self.cwd}:/opt/_avocado/src:ro",
                "-v",
                f"{self.cwd}/_avocado:/opt/_avocado:rw",
            ]
        )

        # Environment variables
        if target:
            container_cmd.extend(["-e", f"AVOCADO_SDK_TARGET={target}"])

        if env_vars:
            for key, value in env_vars.items():
                container_cmd.extend(["-e", f"{key}={value}"])

        # Add additional container arguments if provided
        if container_args:
            container_cmd.extend(container_args)

        # Add the container image
        container_cmd.append(container_image)

        # Add the command to execute
        if isinstance(command, str):
            container_cmd.append(command)
        else:
            container_cmd.extend(command)

        return container_cmd

    def _execute_container_command(
        self, container_cmd: List[str], detach: bool = False, verbose: bool = False
    ) -> bool:
        try:
            if verbose:
                print(f"Mounting host directory: {self.cwd} -> /opt\n")
                print(f"Container command: {' '.join(container_cmd)}")

            if detach:
                result = subprocess.run(
                    container_cmd, check=True, capture_output=True, text=True
                )
                container_id = result.stdout.strip()
                print(
                    f"Container started in detached mode with ID: {
                        container_id}"
                )
                return True
            else:
                result = subprocess.run(container_cmd, check=False)
                return result.returncode == 0

        except KeyboardInterrupt:
            print(
                "\nINFO: Keyboard interrupt received. Container process may also be stopping."
            )
            return False
        except subprocess.CalledProcessError as e:
            print_error(f"Container execution failed: {e}")
            if hasattr(e, "stdout") and e.stdout:
                print(f"STDOUT: {e.stdout}", file=sys.stderr)
            if hasattr(e, "stderr") and e.stderr:
                print(f"STDERR: {e.stderr}", file=sys.stderr)
            return False
        except FileNotFoundError:
            print_error(
                f"{
                    self.container_tool} command not found. Is it installed and in your PATH?"
            )
            return False

    def _create_entrypoint_script(self, source_environment: bool = True) -> str:
        script = """
set -e

# Get repo url from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_URL" ]; then
    REPO_URL="$AVOCADO_SDK_REPO_URL"
else
    REPO_URL="https://repo.avocadolinux.org"
fi

echo $REPO_URL

# Get repo release from environment or default to prod
if [ -n "$AVOCADO_SDK_REPO_RELEASE" ]; then
    REPO_RELEASE="$AVOCADO_SDK_REPO_RELEASE"
else
    REPO_RELEASE="https://repo.avocadolinux.org"

    # Read VERSION_CODENAME from os-release, defaulting to "dev" if not found
    if [ -f /etc/os-release ]; then
        REPO_RELEASE=$(grep "^VERSION_CODENAME=" /etc/os-release | cut -d= -f2 | tr -d '"')
    fi
    REPO_RELEASE=${REPO_RELEASE:-dev}
fi

echo $REPO_RELEASE

export AVOCADO_PREFIX="/opt/_avocado/${AVOCADO_SDK_TARGET}"
export AVOCADO_SDK_PREFIX="${AVOCADO_PREFIX}/sdk"
export AVOCADO_EXT_SYSROOTS="${AVOCADO_PREFIX}/extensions"
export DNF_SDK_HOST_PREFIX="${AVOCADO_SDK_PREFIX}"
export DNF_SDK_TARGET_PREFIX="${AVOCADO_SDK_PREFIX}/target-repoconf"
export DNF_SDK_HOST="\
dnf \
--releasever="$REPO_RELEASE" \
--best \
--setopt=tsflags=noscripts \
"

export DNF_SDK_HOST_OPTS="\
--setopt=cachedir=${DNF_SDK_HOST_PREFIX}/var/cache \
--setopt=logdir=${DNF_SDK_HOST_PREFIX}/var/log \
--setopt=persistdir=${DNF_SDK_HOST_PREFIX}/var/lib/dnf
"

export DNF_SDK_HOST_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_HOST_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_HOST_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export DNF_SDK_TARGET_REPO_CONF="\
--setopt=varsdir=${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars \
--setopt=reposdir=${DNF_SDK_TARGET_PREFIX}/etc/yum.repos.d \
"

export RPM_NO_CHROOT_FOR_SCRIPTS=1

mkdir -p /etc/dnf/vars
mkdir -p ${AVOCADO_SDK_PREFIX}/etc/dnf/vars
mkdir -p ${AVOCADO_SDK_PREFIX}/target-repoconf/etc/dnf/vars

echo "${REPO_URL}" > /etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_HOST_PREFIX}/etc/dnf/vars/repo_url
echo "${REPO_URL}" > ${DNF_SDK_TARGET_PREFIX}/etc/dnf/vars/repo_url

if [ ! -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    echo "[INFO] Initializing Avocado SDK."
    mkdir -p $AVOCADO_SDK_PREFIX/etc
    mkdir -p $AVOCADO_EXT_SYSROOTS
    cp /etc/rpmrc $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/rpm $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/dnf $AVOCADO_SDK_PREFIX/etc
    cp -r /etc/yum.repos.d $AVOCADO_SDK_PREFIX/etc

    mkdir -p $AVOCADO_SDK_PREFIX/usr/lib/rpm
    cp -r /usr/lib/rpm/* $AVOCADO_SDK_PREFIX/usr/lib/rpm/

    # Before calling DNF, $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros needs to be updated to point:
    #   - /usr -> $AVOCADO_SDK_PREFIX/usr
    #   - /var -> $AVOCADO_SDK_PREFIX/var
    sed -i "s|^%_usr[[:space:]]*/usr$|%_usr                   $AVOCADO_SDK_PREFIX/usr|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros
    sed -i "s|^%_var[[:space:]]*/var$|%_var                   $AVOCADO_SDK_PREFIX/var|" $AVOCADO_SDK_PREFIX/usr/lib/rpm/macros


    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_HOST_REPO_CONF -y install "avocado-sdk-$AVOCADO_SDK_TARGET"

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF check-update

    RPM_CONFIGDIR="$AVOCADO_SDK_PREFIX/usr/lib/rpm" \
        RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX" \
        $DNF_SDK_HOST $DNF_SDK_HOST_OPTS $DNF_SDK_REPO_CONF -y install avocado-sdk-toolchain

    echo "[INFO] Installing rootfs sysroot."
    RPM_ETCCONFIGDIR="$DNF_SDK_TARGET_PREFIX" \
      $DNF_SDK_HOST $DNF_SDK_TARGET_REPO_CONF \
      -y --installroot $AVOCADO_PREFIX/rootfs install avocado-pkg-rootfs

    echo "[INFO] Installing SDK target sysroot."
    RPM_ETCCONFIGDIR=$DNF_SDK_TARGET_PREFIX \
    $DNF_SDK_HOST \
        $DNF_SDK_TARGET_REPO_CONF \
        -y \
        --installroot ${AVOCADO_SDK_PREFIX}/target-sysroot \
        install \
        packagegroup-core-standalone-sdk-target
fi

export RPM_ETCCONFIGDIR="$AVOCADO_SDK_PREFIX"

cd /opt/_avocado/src
"""

        if source_environment:
            script += """
# Source the environment setup if it exists
if [ -f "${AVOCADO_SDK_PREFIX}/environment-setup" ]; then
    source "${AVOCADO_SDK_PREFIX}/environment-setup"
fi
"""
        return script
