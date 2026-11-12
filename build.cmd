@echo off
setlocal
rem Использование: build.cmd [debug] — по умолчанию release.
rem Финальный exe копируется в bin\miss.exe в корне проекта.

set OUT=target\release
set ARGS=--release
if /i "%~1"=="debug" (
    set OUT=target\debug
    set ARGS=
)

cargo build %ARGS%
if errorlevel 1 exit /b 1

if not exist bin mkdir bin
copy /y "%OUT%\miss.exe" "bin\miss.exe" >nul
if errorlevel 1 exit /b 1

echo OK: bin\miss.exe
endlocal
