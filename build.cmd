@echo off
rem Builds the workspace inside a Visual Studio 2022 developer environment (any edition with
rem the C++ tools; found with vswhere, so a newer VS 18 install isn't picked by mistake).
rem     build.cmd build --release          the same arguments as cargo
rem     build.cmd test --workspace
rem After every successful release build desklink.exe, dl-probe.exe and README.md are copied
rem into dist\. A DeskLink running from dist\ is stopped for the copy and started again.
setlocal
cd /d "%~dp0"

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "VS="
if exist "%VSWHERE%" for /f "usebackq delims=" %%i in (`"%VSWHERE%" -products * -version [17.0^,18.0^) -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -latest -property installationPath`) do set "VS=%%i"
if not defined VS (echo Visual Studio 2022 with "Desktop development with C++" was not found. & exit /b 1)
call "%VS%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>nul || (echo Couldn't set up "%VS%" & exit /b 1)

cargo %*
if errorlevel 1 exit /b 1

set "RELEASE="
for %%a in (%*) do if /i "%%~a"=="--release" set "RELEASE=1"
if not defined RELEASE exit /b 0
if not exist target\release\desklink.exe exit /b 0

if not exist dist mkdir dist
set "RESTART="
if exist dist\desklink.exe (
    tasklist /FI "IMAGENAME eq desklink.exe" 2>nul | find /i "desklink.exe" >nul && (
        set "RESTART=1"
        dist\desklink.exe stop >nul
    )
)
rem The exe stays locked for a moment after DeskLink exits.
set /a TRIES=0
:copy
copy /y target\release\desklink.exe dist\ >nul 2>nul && goto copied
set /a TRIES+=1
if %TRIES% geq 10 (echo Could not copy into dist\ - is desklink.exe still running? ^(tray icon ^> Exit^) & exit /b 1)
timeout /t 1 /nobreak >nul
goto copy
:copied
if exist target\release\dl-probe.exe copy /y target\release\dl-probe.exe dist\ >nul
copy /y README.md dist\ >nul
echo dist\ updated.
if defined RESTART (
    start "" "%~dp0dist\desklink.exe"
    echo DeskLink started again.
)
