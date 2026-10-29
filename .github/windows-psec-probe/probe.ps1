# Bring Windows 11 up to the first build with MXC process isolation
# (26100/26200.9278, KB5120998), then check whether deltabox can start a
# PSEC-sandboxed command. Results go to the shared folder (\\host.lan\Data),
# which the workflow polls for done.txt.
$ErrorActionPreference = 'Continue'
$share = '\\host.lan\Data'
for ($i = 0; $i -lt 60 -and -not (Test-Path $share); $i++) { Start-Sleep 5 }

function Log($message) {
    $line = "$(Get-Date -Format o) $message"
    Add-Content C:\OEM\probe.log $line
    try { Add-Content "$share\probe.log" $line } catch {}
}

function Finish($result) {
    Set-Content "$share\result.txt" $result
    schtasks.exe /Delete /TN DeltaboxProbe /F | Out-Null
    Set-Content "$share\done.txt" 'done'
    exit
}

$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$build = "$($cv.CurrentBuild).$($cv.UBR)"
Log "$($cv.ProductName) ($($cv.EditionID)) $($cv.DisplayVersion), build $build"

$roundsFile = 'C:\OEM\update-rounds.txt'
$rounds = if (Test-Path $roundsFile) { [int](Get-Content $roundsFile) } else { 0 }
$needsUpdate = ([int]$cv.CurrentBuild -in 26100, 26200) -and ([int]$cv.UBR -lt 9278)
if ($needsUpdate -and $rounds -ge 4) {
    Log "still below build 9278 after $rounds update rounds"
}
if ($needsUpdate -and $rounds -lt 4) {
    Set-Content $roundsFile ($rounds + 1)
    Log "update round $($rounds + 1)"
    # dockur pauses Windows Update; lift the pause for this search.
    $ux = 'HKLM:\SOFTWARE\Microsoft\WindowsUpdate\UX\Settings'
    Get-Item $ux -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Property |
        Where-Object { $_ -like 'Pause*' -or $_ -eq 'FlightSettingsMaxPauseDays' } |
        ForEach-Object { Remove-ItemProperty $ux -Name $_ -ErrorAction SilentlyContinue }
    Set-Service wuauserv -StartupType Manual
    Start-Service wuauserv
    try {
        $session = New-Object -ComObject Microsoft.Update.Session
        $found = $session.CreateUpdateSearcher().Search("IsInstalled=0 and Type='Software' and IsHidden=0")
        $updates = New-Object -ComObject Microsoft.Update.UpdateColl
        foreach ($update in $found.Updates) {
            if ($update.Title -match 'Security Intelligence|Malicious Software Removal') { continue }
            $update.AcceptEula()
            [void]$updates.Add($update)
            Log "  queued: $($update.Title)"
        }
        if ($updates.Count -gt 0) {
            $downloader = $session.CreateUpdateDownloader()
            $downloader.Updates = $updates
            Log "  download result: $($downloader.Download().ResultCode)"
            $installer = $session.CreateUpdateInstaller()
            $installer.Updates = $updates
            $installed = $installer.Install()
            Log "  install result: $($installed.ResultCode), reboot required: $($installed.RebootRequired)"
            if ($installed.RebootRequired) {
                Restart-Computer -Force
                exit
            }
        } else {
            Log '  no applicable updates found'
        }
    } catch {
        Log "  Windows Update failed: $_"
    }
    $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
    $build = "$($cv.CurrentBuild).$($cv.UBR)"
}

# copy-private needs no WinFsp mount, and its exec path checks PSEC and the
# PSE_SUPPORT_FS_DENY capability before launching the command.
$project = 'C:\probe-project'
New-Item -ItemType Directory -Force $project | Out-Null
Set-Content "$project\a.txt" 'psec-probe-ok'
$output = & C:\OEM\deltabox.exe run --project $project --fs-mode copy-private --abort -- cmd /c type a.txt 2>&1 | Out-String
$code = $LASTEXITCODE
Log "probe exit code $code"
Log $output
Finish "build=$build`nexit=$code`n$output"
