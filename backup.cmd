@echo off
rem Zips everything needed to rebuild DeskLink (sources, build scripts, docs, screenshots)
rem into backups\DeskLink_<date>_<time>.zip, or into the folder given as the first argument:
rem     backup.cmd                 -> backups\DeskLink_20261116_101500.zip
rem     backup.cmd D:\Backups      -> D:\Backups\DeskLink_20261116_101500.zip
rem Skips build output (target\), dist\ (it holds THIS PC's identity, desklink.key and
rem desklink.cert, which must never be copied to another PC) and earlier backups.
setlocal
cd /d "%~dp0"

set "OUTDIR=%~1"
if "%OUTDIR%"=="" set "OUTDIR=%~dp0backups"
if not exist "%OUTDIR%" mkdir "%OUTDIR%" || (echo Can't create "%OUTDIR%" & exit /b 1)

for /f %%t in ('powershell -NoProfile -Command "Get-Date -Format yyyyMMdd_HHmmss"') do set "STAMP=%%t"
set "ZIP=%OUTDIR%\DeskLink_%STAMP%.zip"

set "EXTRA="
if exist screenshots set "EXTRA=screenshots"
if exist .cargo set "EXTRA=%EXTRA% .cargo"

rem Windows' own tar.exe (bsdtar, ships with Windows 10/11): -a picks zip from the extension.
rem Called by full path so Git's GNU tar on PATH (which can't write zips) isn't used instead.
"%SystemRoot%\System32\tar.exe" -a -c -f "%ZIP%" ^
    --exclude "*/target" --exclude "*.tmp" --exclude "*.pdb" ^
    Cargo.toml Cargo.lock crates build.cmd dist.cmd build_publish_exe.cmd backup.cmd README.md HANDOVER.md .gitignore .gitattributes tools %EXTRA%
if errorlevel 1 (
    echo Backup FAILED.
    if exist "%ZIP%" del "%ZIP%"
    exit /b 1
)

for %%f in ("%ZIP%") do echo Backup written: %%~ff  (%%~zf bytes)
endlocal
