@echo off
rem Release build -> desklink.zip -> GitHub Release (tag = v<build version>).
rem     build_publish_exe.cmd        asks before publishing
rem     build_publish_exe.cmd -y     publishes without asking
rem The tag is the exe's own version stamp (UTC build time, see crates\desklink\build.rs), so
rem DeskLink's self-update sees every published build as newer. Needs the GitHub CLI (gh),
rem logged in.
setlocal
cd /d "%~dp0"

where gh >nul 2>nul || (echo GitHub CLI ^(gh^) is not installed. & exit /b 1)
gh auth status >nul 2>nul || (echo gh is not logged in - run: gh auth login & exit /b 1)

rem Release build into target\release (build.cmd also refreshes dist\).
call "%~dp0build.cmd" build --release -p desklink || (echo Build FAILED. & exit /b 1)

set "EXE=%~dp0target\release\desklink.exe"
set "VERSION="
for /f "delims=" %%v in ('"%EXE%" --version') do set "VERSION=%%v"
echo %VERSION%| findstr /r "^[0-9][0-9][0-9][0-9]\.[0-9][0-9]\.[0-9][0-9]-[0-9][0-9][0-9][0-9]$" >nul
if errorlevel 1 (echo target\release\desklink.exe doesn't report a version ^(got "%VERSION%"^). & exit /b 1)
set "TAG=v%VERSION%"

gh release view %TAG% >nul 2>nul && (echo Release %TAG% already exists - build again for a new version. & exit /b 1)

rem The release tag is created on the remote's default branch, so local changes aren't in it.
git status --porcelain | findstr . >nul && echo Warning: uncommitted changes in the working tree.
git log @{u}.. --oneline 2>nul | findstr . >nul && echo Warning: unpushed commits - the tag will point at the remote HEAD.

rem The asset must be named desklink.zip (crates\desklink\src\update.rs and the README link
rem look for it).
set "STAGE=%TEMP%\desklink_release"
if exist "%STAGE%" rmdir /s /q "%STAGE%"
mkdir "%STAGE%" || (echo Can't create "%STAGE%" & exit /b 1)
set "ZIP=%STAGE%\desklink.zip"
rem Windows' own tar.exe (bsdtar) by full path: Git's GNU tar can't write zips.
"%SystemRoot%\System32\tar.exe" -a -c -f "%ZIP%" -C "%~dp0target\release" desklink.exe
if errorlevel 1 (echo Zip FAILED. & rmdir /s /q "%STAGE%" & exit /b 1)

for %%f in ("%ZIP%") do echo Release %TAG%  (desklink.zip, %%~zf bytes)
if /i not "%~1"=="-y" (
    set "ANSWER="
    set /p "ANSWER=Publish? [Y/n] "
    call :cancelled && (rmdir /s /q "%STAGE%" & echo Cancelled. & exit /b 0)
)

gh release create %TAG% "%ZIP%" --title %TAG% --generate-notes
set "CODE=%ERRORLEVEL%"
rmdir /s /q "%STAGE%"
if not "%CODE%"=="0" (echo Publish FAILED. & exit /b %CODE%)

echo.
echo Published %TAG%
echo Download: https://github.com/zeroskew/DeskLink/releases/latest/download/desklink.zip
endlocal
exit /b 0

:cancelled
rem Succeeds (errorlevel 0) when the answer was n/no.
if /i "%ANSWER%"=="n" exit /b 0
if /i "%ANSWER%"=="no" exit /b 0
exit /b 1
