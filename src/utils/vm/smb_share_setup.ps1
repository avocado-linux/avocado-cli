# Avocado VM workspace share: one-time host setup (needs elevation).
#
# Windows builds of QEMU cannot export a host directory to the guest (no
# virtio-9p), so on Windows the avocado-vm mounts the workspace over SMB
# from the host's own file server, reached through the VM's user-mode
# network. This script creates what that needs:
#
#   1. a local account used only for this share, with a random password;
#   2. Modify rights for that account on the workspace folder;
#   3. an SMB share of the workspace folder, open to that account alone.
#
# The password is written to -CredentialFile encrypted with DPAPI for the
# current user, so only this Windows user can read it back. It is never
# printed. Re-running is safe: it resets the password and re-creates the
# share for the given folder.
param(
    [Parameter(Mandatory = $true)][string]$Workspace,
    [Parameter(Mandatory = $true)][string]$CredentialFile,
    [string]$User = 'avocado-vm',
    [string]$Share = 'avocado-workspace',
    [string]$ResultFile = ''
)
$ErrorActionPreference = 'Stop'

function Finish([string]$status) {
    if ($ResultFile) { Set-Content -Path $ResultFile -Value $status -Encoding ASCII }
}

try {
    if (-not (Test-Path -LiteralPath $Workspace -PathType Container)) {
        throw "workspace folder '$Workspace' does not exist"
    }

    $bytes = New-Object byte[] 24
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    # URL-safe base64: no characters that need escaping in a mount option.
    $plain = [Convert]::ToBase64String($bytes).Replace('+', '-').Replace('/', '_')
    $secure = ConvertTo-SecureString $plain -AsPlainText -Force

    if (Get-LocalUser -Name $User -ErrorAction SilentlyContinue) {
        Set-LocalUser -Name $User -Password $secure
    } else {
        New-LocalUser -Name $User -Password $secure -PasswordNeverExpires `
            -UserMayNotChangePassword -AccountNeverExpires `
            -Description 'Avocado VM workspace share (SMB only)' | Out-Null
    }

    # Keep the account off the sign-in screen; it exists for SMB only.
    $hide = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList'
    if (-not (Test-Path $hide)) { New-Item -Path $hide -Force | Out-Null }
    New-ItemProperty -Path $hide -Name $User -Value 0 -PropertyType DWord -Force | Out-Null

    # Modify on the workspace, inherited by everything under it.
    & icacls $Workspace /grant "${User}:(OI)(CI)M" /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "icacls failed with exit code $LASTEXITCODE" }

    if (Get-SmbShare -Name $Share -ErrorAction SilentlyContinue) {
        Remove-SmbShare -Name $Share -Force
    }
    New-SmbShare -Name $Share -Path $Workspace -FullAccess "$env:COMPUTERNAME\$User" `
        -Description 'Avocado VM workspace' | Out-Null

    # DPAPI (current user) protected; readable only by this Windows user.
    $dir = Split-Path -Parent $CredentialFile
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    ConvertFrom-SecureString $secure | Set-Content -Path $CredentialFile -Encoding ASCII

    Finish 'ok'
} catch {
    Finish "error: $($_.Exception.Message)"
    exit 1
}
