# Avocado VM workspace share: one-time host setup (needs elevation).
#
# Windows builds of QEMU cannot export a host directory to the guest (no
# virtio-9p), so on Windows the avocado-vm mounts the workspace over SMB
# from the host's own file server, reached through the VM's user-mode
# network. This script creates what that needs:
#
#   1. a local account used only for this share;
#   2. Modify rights for that account on the workspace folder;
#   3. an SMB share of the workspace folder, open to that account alone.
#
# The account's password is chosen by the avocado CLI, which also keeps it
# (DPAPI-encrypted for the user who ran the CLI). This script may run as a
# different administrator, when a standard user approves the prompt with
# someone else's credentials, so it stores nothing itself: it reads the
# password from -PasswordFile and deletes that file at once. The password
# is never printed. Re-running is safe: it sets the password again and
# re-creates the share for the given folder.
param(
    [Parameter(Mandatory = $true)][string]$Workspace,
    [Parameter(Mandatory = $true)][string]$PasswordFile,
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

    $plain = ([IO.File]::ReadAllText($PasswordFile)).Trim()
    Remove-Item -LiteralPath $PasswordFile -Force
    if (-not $plain) { throw 'no password was supplied' }
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

    Finish 'ok'
} catch {
    if (Test-Path -LiteralPath $PasswordFile) { Remove-Item -LiteralPath $PasswordFile -Force }
    Finish "error: $($_.Exception.Message)"
    exit 1
}
