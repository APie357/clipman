-- watermark: draw a line of text on exported clips with ffmpeg's drawtext filter.
-- Only compress presets re-encode the video, so source presets are left alone.

local positions = {
  ["Bottom right"] = "x=w-tw-24:y=h-th-24",
  ["Bottom left"] = "x=24:y=h-th-24",
  ["Top right"] = "x=w-tw-24:y=24",
  ["Top left"] = "x=24:y=24",
}

return {
  name = "Watermark",
  version = "1.0",
  api = 1,
  description = "Text on exported clips",

  settings = {
    { key = "text", label = "Text", default = "",
      help = "Leave empty for no watermark" },
    { key = "position", label = "Position", type = "choice",
      options = { "Bottom right", "Bottom left", "Top right", "Top left" } },
    { key = "size", label = "Size (% of height)", type = "number", default = 4, min = 1, max = 20 },
  },

  on_export_start = function(job)
    local text = clipman.settings.text
    if text == "" or job.kind ~= "compress" then return nil end
    local size = math.floor(clipman.settings.size * 10 + 0.5) / 1000
    return {
      -- expansion=none: % is just a character; filter_escape handles ' : \ , etc.
      vf = "drawtext=expansion=none:text=" .. clipman.filter_escape(text)
        .. ":fontsize=h*" .. size
        .. ":fontcolor=white@0.8:borderw=2:bordercolor=black@0.5:"
        .. positions[clipman.settings.position],
      meta = { text = text },
    }
  end,
}
