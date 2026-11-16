@echo off
rem Builds the release binaries; build.cmd copies them (and README.md) into dist\.
cd /d "%~dp0"
call "%~dp0build.cmd" build --release || exit /b 1
echo dist\ ready:
dir /b dist
