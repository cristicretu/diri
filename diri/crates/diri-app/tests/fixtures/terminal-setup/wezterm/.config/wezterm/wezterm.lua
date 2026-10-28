local wezterm = require 'wezterm'
local config = wezterm.config_builder()

config.font = wezterm.font('Iosevka Term', { weight = 'Medium' })
config.font_size = 13.5 -- a comment = 99
config.line_height = 1.3
config.color_scheme = 'My Scheme'
config.default_cursor_style = 'SteadyUnderline'
config.send_composed_key_when_left_alt_is_pressed = false
config.scrollback_lines = 20000
config.window_padding = {
  left = 2,
  right = 2,
}
config.font_size = os.getenv('BIG') and 20 or 13.5
config.keys = {
  { key = 'd', mods = 'CMD', action = wezterm.action.SplitHorizontal },
}

wezterm.on('gui-startup', function(cmd)
  config.font_size = 30
end)

if wezterm.target_triple == 'x86_64-pc-windows-msvc' then
  config.line_height = 2.0
end

config.enable_tab_bar = false
config.set_environment_variables = { API_KEY = 'sk-live-secret' }

return config
