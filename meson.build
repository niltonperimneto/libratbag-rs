project('libratbag', 'rust',
  version : '1.0.0',
  license : ['GPL-3.0-or-later', 'MIT'],
  meson_version : '>= 0.50.0')

project_source_root = meson.project_source_root()

# DBus API version (must remain in sync with client expectations)
ratbagd_api_version = 2

enable_systemd = get_option('systemd')
if enable_systemd
  dep_systemd = dependency('systemd')
endif

cargo = find_program('cargo', required: true)
cargo_path = cargo.full_path()

ratbagd_src = join_paths(project_source_root, 'ratbagd-rs')
ratbagctl_src = join_paths(project_source_root, 'ratbagctl-rs')

custom_target(
  'ratbagd',
  build_by_default: true,
  command: [
    'sh', '-c',
    '_out="$PWD/$1" && cd "' + ratbagd_src + '" && "' + cargo_path + '" build --release --locked && cp target/release/ratbagd-rs "$_out"',
    '_', '@OUTPUT@',
  ],
  output: 'ratbagd',
  install: true,
  install_dir: get_option('sbindir'),
)

custom_target(
  'ratbagctl',
  build_by_default: true,
  command: [
    'sh', '-c',
    '_out="$PWD/$1" && cd "' + ratbagctl_src + '" && "' + cargo_path + '" build --release --locked && cp target/release/ratbagctl "$_out"',
    '_', '@OUTPUT@',
  ],
  output: 'ratbagctl',
  install: true,
  install_dir: get_option('bindir'),
)

install_man('ratbagd/ratbagd.8')

# Install device database (.device files)
install_subdir('data/devices',
  strip_directory : true,
  exclude_files : ['device.example', 'README.md'],
  install_dir : join_paths(get_option('datadir'), 'libratbag'))

config_sbindir = configuration_data()
config_sbindir.set('sbindir', join_paths(get_option('prefix'), get_option('sbindir')))

# systemd unit file (DBus-activated)
if enable_systemd
  unitdir = get_option('systemd-unit-dir')
  if unitdir == ''
    default_unitdir = dep_systemd.get_pkgconfig_variable('systemdsystemunitdir')
    intended_unitdir = join_paths(get_option('prefix'), get_option('libdir'), 'systemd')
    if get_option('prefix') == '/usr' and intended_unitdir != default_unitdir
      message('systemd unitdir libdir mismatch detected, using @0@'.format(default_unitdir))
      unitdir = default_unitdir
    else
      unitdir = intended_unitdir
    endif
  endif

  configure_file(
    input : 'ratbagd/ratbagd.service.in',
    output : 'ratbagd.service',
    configuration : config_sbindir,
    install_dir : unitdir,
  )
endif

dbusdir = get_option('dbus-root-dir')
if dbusdir == ''
  dbusdir = join_paths(get_option('prefix'), get_option('datadir'), 'dbus-1')
endif

configure_file(
  input : 'dbus/org.freedesktop.ratbag1.service.in',
  output : 'org.freedesktop.ratbag1.service',
  configuration : config_sbindir,
  install_dir : join_paths(dbusdir, 'system-services')
)

dbusgroup = get_option('dbus-group')
if dbusgroup == ''
  access = 'context="default"'
else
  access = 'group="@0@"'.format(dbusgroup)
endif

config_dbusaccess = configuration_data()
config_dbusaccess.set('access', access)

configure_file(
  input : 'dbus/org.freedesktop.ratbag1.conf.in',
  output : 'org.freedesktop.ratbag1.conf',
  configuration : config_dbusaccess,
  install_dir : join_paths(dbusdir, 'system.d'))
