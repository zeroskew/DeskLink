# Takes the README screenshots: opens the settings window on each tab with the demo settings
# (tools\screenshot-demo.cfg: made-up PCs, nothing from this PC's config) and captures it with
# PrintWindow. No input is sent; each window is closed again.
#     powershell -File tools\screenshots.ps1 [-Exe target\release\desklink.exe]
param([string]$Exe = "target\release\desklink.exe")
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Set-Location $root
$Exe = (Resolve-Path $Exe).Path

# The settings window is single-instance: a second one would just bring the user's to the front.
if (Get-Process desklink -ErrorAction SilentlyContinue | Where-Object MainWindowTitle -eq 'DeskLink') {
    Write-Host "A DeskLink settings window is open - close it first." -ForegroundColor Red; exit 1
}

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class PW {
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int size);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int n);
  // The process's visible top-level window with this title (the biggest one).
  public static IntPtr Find(uint pid, string title) {
    IntPtr best = IntPtr.Zero; long area = 0;
    EnumWindows((h, l) => {
      uint p; GetWindowThreadProcessId(h, out p);
      if (p != pid || !IsWindowVisible(h)) return true;
      var sb = new System.Text.StringBuilder(256); GetWindowText(h, sb, 256);
      if (sb.ToString() != title) return true;
      RECT r; GetWindowRect(h, out r);
      long a = (long)(r.R - r.L) * (r.B - r.T);
      if (a > area) { area = a; best = h; }
      return true;
    }, IntPtr.Zero);
    return best;
  }
  public struct RECT { public int L, T, R, B; }
}
"@
[PW]::SetProcessDPIAware() | Out-Null

# The demo config is copied, so the window's saves never touch the one in tools\.
$work = Join-Path $env:TEMP "desklink-screenshots"
New-Item -ItemType Directory -Force $work | Out-Null
New-Item -ItemType Directory -Force (Join-Path $root 'screenshots') | Out-Null

$shots = [ordered]@{ layout = 'layout'; devices = 'devices'; settings = 'settings'; security = 'security' }
foreach ($page in $shots.Keys) {
    Copy-Item tools\screenshot-demo.cfg (Join-Path $work 'desklink.cfg') -Force
    # A throwaway identity: otherwise DeskLink's first-run migration copies this PC's real one
    # from %APPDATA%\DeskLink into the folder (and the window shows its fingerprint).
    foreach ($f in 'desklink.cert', 'desklink.key') {
        $bytes = New-Object byte[] 352; [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
        [IO.File]::WriteAllBytes((Join-Path $work $f), $bytes)
    }
    $p = Start-Process $Exe -ArgumentList @('settings', '--config', (Join-Path $work 'desklink.cfg'), '--page', $page) -PassThru
    $h = [IntPtr]::Zero
    for ($i = 0; $i -lt 50 -and $h -eq [IntPtr]::Zero; $i++) { Start-Sleep -Milliseconds 200; $h = [PW]::Find([uint32]$p.Id, 'DeskLink') }
    if ($h -eq [IntPtr]::Zero) { Write-Host "No window for $page" -ForegroundColor Red; $p | Stop-Process -Force; continue }
    Start-Sleep -Milliseconds 1500   # let it draw (and the layout settle)
    # The visible frame (without the invisible resize borders).
    $r = New-Object PW+RECT
    [PW]::DwmGetWindowAttribute($h, 9, [ref]$r, 16) | Out-Null   # DWMWA_EXTENDED_FRAME_BOUNDS
    $w = New-Object PW+RECT
    [PW]::GetWindowRect($h, [ref]$w) | Out-Null
    $full = New-Object System.Drawing.Bitmap ($w.R - $w.L), ($w.B - $w.T)
    $g = [System.Drawing.Graphics]::FromImage($full)
    $dc = $g.GetHdc()
    [PW]::PrintWindow($h, $dc, 2) | Out-Null   # PW_RENDERFULLCONTENT
    $g.ReleaseHdc($dc); $g.Dispose()
    $crop = New-Object System.Drawing.Rectangle ($r.L - $w.L), ($r.T - $w.T), ($r.R - $r.L), ($r.B - $r.T)
    $out = Join-Path $root "screenshots\$($shots[$page]).png"
    $full.Clone($crop, $full.PixelFormat).Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
    $full.Dispose()
    $p | Stop-Process -Force
    $p.WaitForExit()
    Write-Host "screenshots\$($shots[$page]).png"
}
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
