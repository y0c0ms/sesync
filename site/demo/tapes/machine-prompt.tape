Output "/var/tmp/sesync-demo/out/machine-prompt.mp4"
Source common.tape
Set Height 620
Hide
Type ". /var/tmp/sesync-demo/env.sh; cd ~; clear" Enter
Sleep 700ms
Show
Sleep 400ms
Type "sesync machine list laptop" Sleep 400ms Enter Sleep 2.5s
Type 'sesync machine prompt laptop "health check" "Run the tests again" --open --wait' Sleep 500ms Enter
Sleep 8s
Type 'sesync machine read laptop "health check" 4' Sleep 400ms Enter Sleep 4s
Type "clear; sesync machine list laptop" Sleep 400ms Enter Sleep 3.5s
