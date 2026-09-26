@echo off
rem Cortex - interactive menu for Windows (optional helper).
rem Uses `cortex` from the PATH, else %USERPROFILE%\.cortex\bin\cortex.exe, else the repo build.
setlocal enabledelayedexpansion
chcp 65001 >nul
title Cortex

set "REPO=%~dp0..\"
set "CORTEX=cortex"
where cortex >nul 2>nul
if errorlevel 1 (
  set "CORTEX=%USERPROFILE%\.cortex\bin\cortex.exe"
  if not exist "!CORTEX!" set "CORTEX=%REPO%target\release\cortex.exe"
)

:menu
cls
echo.
echo   ============================================================
echo                         C O R T E X
echo   ============================================================
echo.
echo     1.  Find code                     (find)
echo     2.  Symbol card                   (card)
echo     3.  Impact of a change            (impact)
echo     4.  Search offline docs           (docs query)
echo     5.  Scrape docs (batch)           (docs batch)
echo     6.  Open the 3D viewer            (viewer)
echo     7.  Update everything             (update-all)
echo     8.  Rebuild Cortex                (build.ps1 release)
echo     9.  List projects / docs          (list / docs list)
echo     0.  Quit
echo.
set /p "choice=  Choice: "

if "%choice%"=="1" goto find
if "%choice%"=="2" goto card
if "%choice%"=="3" goto impact
if "%choice%"=="4" goto docsquery
if "%choice%"=="5" goto scrape
if "%choice%"=="6" goto viewer
if "%choice%"=="7" goto updateall
if "%choice%"=="8" goto rebuild
if "%choice%"=="9" goto list
if "%choice%"=="0" exit /b
goto menu

:find
set /p "q=  Question: "
set /p "p=  Project (empty = all): "
if "%p%"=="" ("%CORTEX%" find "%q%") else ("%CORTEX%" find "%q%" -p %p%)
pause & goto menu

:card
set /p "sym=  Symbol or id: "
set /p "p=  Project (empty = all): "
if "%p%"=="" ("%CORTEX%" card "%sym%") else ("%CORTEX%" card "%sym%" -p %p%)
pause & goto menu

:impact
set /p "sym=  Symbol or id: "
set /p "p=  Project (empty = all): "
if "%p%"=="" ("%CORTEX%" impact "%sym%") else ("%CORTEX%" impact "%sym%" -p %p%)
pause & goto menu

:docsquery
set /p "q=  Question: "
"%CORTEX%" docs query "%q%" -b 1500
pause & goto menu

:scrape
set /p "cfg=  Config file [examples\docs-example.txt]: "
if "%cfg%"=="" set "cfg=%REPO%examples\docs-example.txt"
"%CORTEX%" docs batch "%cfg%" --concurrency 16
pause & goto menu

:viewer
"%CORTEX%" viewer
goto menu

:updateall
"%CORTEX%" update-all
pause & goto menu

:rebuild
pushd "%REPO%"
powershell -ExecutionPolicy Bypass -File "%REPO%build.ps1" release
popd
pause & goto menu

:list
"%CORTEX%" list
echo.
"%CORTEX%" docs list
pause & goto menu
