@echo off
rem Runs once at the VM's first logon (dockur/windows copies this folder to
rem C:\OEM). Keep probe.ps1 running across the reboots its update rounds need.
start "" /wait msiexec.exe /i C:\OEM\winfsp.msi /qn ADDLOCAL=ALL
schtasks.exe /Create /TN DeltaboxProbe /SC ONLOGON /RL HIGHEST /F /TR "powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\OEM\probe.ps1"
powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\OEM\probe.ps1
