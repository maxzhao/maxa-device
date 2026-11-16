# Build a Windows installer without embedding the maintainer's user/build paths.
[CmdletBinding()]
param()
$ErrorActionPreference='Stop'
$root=[IO.Path]::GetFullPath($PSScriptRoot).TrimEnd('\')
$profile=[IO.Path]::GetFullPath($env:USERPROFILE).TrimEnd('\')
$names=@('CARGO_ENCODED_RUSTFLAGS','CFLAGS','CXXFLAGS','CC_SHELL_ESCAPED_FLAGS')
$previous=@{}
foreach($name in $names){$previous[$name]=[Environment]::GetEnvironmentVariable($name,'Process')}
Push-Location -LiteralPath $root
try {
 $separator=[char]31
 $flags=@("--remap-path-prefix=$root=/build/maxa-device","--remap-path-prefix=$profile=/build/user",'-C','link-arg=/PDBALTPATH:maxa-device.pdb')
 $env:CARGO_ENCODED_RUSTFLAGS=($flags -join $separator)
 # MSVC applies /pathmap to __FILE__, including native crypto dependencies.
 $env:CC_SHELL_ESCAPED_FLAGS='1'
 $nativeFlags=' /experimental:deterministic "/pathmap:'+$profile+'=/build/user"'
 $env:CFLAGS=($previous.CFLAGS+$nativeFlags).Trim()
 $env:CXXFLAGS=($previous.CXXFLAGS+$nativeFlags).Trim()
 npm.cmd run tauri -- build --ci -- --locked
 if($LASTEXITCODE -ne 0){throw 'Windows release build failed'}
} finally {
 foreach($name in $names){[Environment]::SetEnvironmentVariable($name,$previous[$name],'Process')}
 Pop-Location
}
