; Windows will not overwrite a running program but lets it be renamed. A backend still running keeps
; its renamed file until the updated app asks it to give way, so no chat is stopped to install.
!macro MEDHA_SET_ASIDE_BACKEND ID
  Delete "$INSTDIR\medha.exe.old-1"
  Delete "$INSTDIR\medha.exe.old-2"
  Delete "$INSTDIR\medha.exe.old-3"
  Delete "$INSTDIR\medha.exe"
  IfFileExists "$INSTDIR\medha.exe" 0 medha_aside_${ID}
  ClearErrors
  Rename "$INSTDIR\medha.exe" "$INSTDIR\medha.exe.old-1"
  IfErrors 0 medha_aside_${ID}
  ClearErrors
  Rename "$INSTDIR\medha.exe" "$INSTDIR\medha.exe.old-2"
  IfErrors 0 medha_aside_${ID}
  ClearErrors
  Rename "$INSTDIR\medha.exe" "$INSTDIR\medha.exe.old-3"
  medha_aside_${ID}:
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro MEDHA_SET_ASIDE_BACKEND install
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro MEDHA_SET_ASIDE_BACKEND uninstall
!macroend

; A renamed backend still running is removed once Windows restarts.
!macro NSIS_HOOK_POSTUNINSTALL
  Delete /REBOOTOK "$INSTDIR\medha.exe.old-1"
  Delete /REBOOTOK "$INSTDIR\medha.exe.old-2"
  Delete /REBOOTOK "$INSTDIR\medha.exe.old-3"
!macroend
