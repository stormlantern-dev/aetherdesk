`probe-hevc420.hevc` and `probe-hevc444.hevc`: one 256×256 HEVC picture each (4:2:0 and 4:4:4), built into the client to find out whether the GPU decodes that format. Made with:

```sh
ffmpeg -f lavfi -i "color=c=0x3366cc:s=256x256" -frames:v 1 -pix_fmt yuv444p \
  -c:v libx265 -preset ultrafast -x265-params "log-level=error:repeat-headers=1" \
  -f hevc probe-hevc444.hevc
```

(and `-pix_fmt yuv420p` for the other). 256×256 because NVDEC won't decode HEVC below 144×144.
