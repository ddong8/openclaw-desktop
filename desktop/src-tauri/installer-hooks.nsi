; Tauri NSIS installer hooks. Kill any running OpenClaw shell + its node
; sidecar children before installation/upgrade, so the installer can overwrite
; files in C:\Program Files\OpenClaw\resources\node\node.exe without "file in
; use" errors.
;
; Notes:
;  - /T kills the process tree, so the embedded portable node sidecar (spawned
;    as a child of openclaw-desktop.exe) is taken down too.
;  - Errors are suppressed; if the process isn't running, killing is a no-op.

!macro NSIS_HOOK_PREINSTALL
  DetailPrint "Stopping any running OpenClaw instance…"
  nsExec::Exec 'taskkill /F /T /IM openclaw-desktop.exe'
  Pop $0 ; discard exit code
  Sleep 1500
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Stopping any running OpenClaw instance…"
  nsExec::Exec 'taskkill /F /T /IM openclaw-desktop.exe'
  Pop $0
  Sleep 1500
!macroend
