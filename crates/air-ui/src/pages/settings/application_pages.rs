use gpui::{Axis, Context, Entity, IntoElement, ParentElement, Styled, div, px};
use gpui_component::animation::{Transition, ease_out_cubic};
use gpui_component::group_box::GroupBoxVariant;
use gpui_component::input::{Input, InputState};
use gpui_component::setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings};
use gpui_component::{Icon as ComponentIcon, IconName};

use air_platform::core_service::CoreServiceSnapshot;
use air_settings::{AppLanguage, AppSettings, CloseWindowBehavior, GuiThemePreference};
use air_ui::pages::config_editor::{self, ConfigBoolField, ConfigEditorGroup, ConfigTextField};
use air_ui::shell::{Shell, ShellPalette};
use air_ui::{components, icons::Icon};

use super::controls::*;
use super::network_pages::{dns_page, sniffer_page};

// 设置页侧栏承载分组导航，按主内容区约 3/10 的视觉比例加宽，长标题不易挤压。
const SETTINGS_SIDEBAR_WIDTH: f32 = 300.0;

#[derive(Clone, Debug)]
pub struct SettingsPageState {
    settings: AppSettings,
}

impl SettingsPageState {
    pub fn new(settings: AppSettings) -> Self {
        Self { settings }
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    pub fn set_theme(&mut self, theme: GuiThemePreference) {
        self.settings.theme = theme;
    }

    pub fn set_bool(&mut self, field: SettingsBoolField, value: bool) {
        match field {
            SettingsBoolField::RestoreWindow => self.settings.restore_window = value,
            SettingsBoolField::StartCoreAfterLaunch => {
                self.settings.start_core_after_launch = value
            }
            SettingsBoolField::Autostart => self.settings.autostart = value,
            SettingsBoolField::SilentStartup => self.settings.silent_start = value,
            SettingsBoolField::SystemProxy => self.settings.system_proxy_enabled = value,
            SettingsBoolField::HideToTray => {
                self.settings.close_window_behavior = if value {
                    CloseWindowBehavior::Tray
                } else {
                    CloseWindowBehavior::Exit
                };
            }
        }
    }

    pub fn set_text(&mut self, field: SettingsTextField, value: impl Into<String>) {
        match field {
            SettingsTextField::ProxyDelayTestUrl => {
                // 应用级测速地址只作为 mihomo delay API 的入参，不写入核心 YAML，避免把运行偏好混入用户核心配置。
                self.settings.proxy_delay_test_url = value.into();
            }
        }
    }

    pub fn toggle_bool(&mut self, field: SettingsBoolField) {
        self.set_bool(field, !self.bool_value(field));
    }

    pub fn view_model(&self) -> SettingsPageViewModel {
        SettingsPageViewModel {
            settings: self.settings.clone(),
            pages: UnifiedSettingsPage::all().to_vec(),
        }
    }

    fn bool_value(&self, field: SettingsBoolField) -> bool {
        match field {
            SettingsBoolField::RestoreWindow => self.settings.restore_window,
            SettingsBoolField::StartCoreAfterLaunch => self.settings.start_core_after_launch,
            SettingsBoolField::Autostart => self.settings.autostart,
            SettingsBoolField::SilentStartup => self.settings.silent_start,
            SettingsBoolField::SystemProxy => self.settings.system_proxy_enabled,
            SettingsBoolField::HideToTray => {
                self.settings.close_window_behavior == CloseWindowBehavior::Tray
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsBoolField {
    RestoreWindow,
    StartCoreAfterLaunch,
    Autostart,
    SilentStartup,
    SystemProxy,
    HideToTray,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsTextField {
    ProxyDelayTestUrl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnifiedSettingsPage {
    Application,
    Core,
    Tun,
    Sniffer,
    Dns,
}

impl UnifiedSettingsPage {
    const ALL: [Self; 5] = [
        Self::Application,
        Self::Core,
        Self::Tun,
        Self::Sniffer,
        Self::Dns,
    ];

    pub fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Application => "应用",
            Self::Core => "内核",
            Self::Tun => "TUN",
            Self::Sniffer => "域名嗅探",
            Self::Dns => "DNS",
        }
    }

    pub(super) fn sidebar_icon(self) -> IconName {
        match self {
            Self::Application => IconName::Settings2,
            Self::Core => IconName::Cpu,
            Self::Tun => IconName::Network,
            Self::Sniffer => IconName::Search,
            Self::Dns => IconName::Globe,
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            Self::Application => "应用偏好写入 app.config.toml，修改后立即生效并自动保存。",
            Self::Core => "mihomo 通用配置写入 core.common.config.yaml；核心运行中会自动重载。",
            Self::Tun => "虚拟网卡配置写入 core.common.config.yaml；核心运行中会自动重载。",
            Self::Sniffer => "域名嗅探配置写入 core.common.config.yaml；核心运行中会自动重载。",
            Self::Dns => "DNS 与 hosts 配置写入 core.common.config.yaml；核心运行中会自动重载。",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettingsPageViewModel {
    pub settings: AppSettings,
    pub pages: Vec<UnifiedSettingsPage>,
}

#[derive(Clone)]
pub(crate) struct SettingsPageInputs {
    pub proxy_delay_test_url: Entity<InputState>,
}

pub(crate) fn render_settings_page(
    state: &SettingsPageState,
    inputs: SettingsPageInputs,
    config_state: &config_editor::ConfigEditorPageState,
    config_inputs: config_editor::ConfigEditorInputs,
    core_service: CoreServiceSnapshot,
    palette: ShellPalette,
    cx: &mut Context<Shell>,
) -> impl IntoElement {
    let shell = cx.entity();
    let settings_model = state.view_model();
    let config_model = config_state.view_model();

    let sidebar_style = div()
        // 设置页嵌在主内容区内，侧栏保持透明，避免和外层页面形成双重底色。
        .bg(palette.page.alpha(0.0))
        .style()
        .clone();
    let hidden_header_style = div()
        .h(px(0.0))
        .max_h(px(0.0))
        .p_0()
        .border_0()
        .overflow_hidden()
        .bg(palette.page)
        .style()
        .clone();

    let content = Settings::new("merged-settings-page")
        .sidebar_width(px(SETTINGS_SIDEBAR_WIDTH))
        .with_group_variant(GroupBoxVariant::Outline)
        .sidebar_style(&sidebar_style)
        .header_style(&hidden_header_style)
        .pages(vec![
            application_page(shell.clone(), settings_model, inputs, core_service, palette),
            core_page(
                shell.clone(),
                config_model.clone(),
                config_inputs.clone(),
                palette,
            ),
            tun_page(
                shell.clone(),
                config_model.clone(),
                config_inputs.clone(),
                palette,
            ),
            sniffer_page(
                shell.clone(),
                config_model.clone(),
                config_inputs.clone(),
                palette,
            ),
            dns_page(shell, config_model, config_inputs, palette),
        ]);

    Transition::new(components::animation_duration(
        components::foundation::PAGE_TRANSITION_MS,
    ))
    .ease(ease_out_cubic)
    .fade(0.0, 1.0)
    .slide_y(px(4.0), px(0.0))
    .apply(
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_w(px(0.0))
            .min_h(px(0.0))
            .child(content),
        "merged-settings-page-enter",
    )
}

pub(super) fn hide_setting_page_header(page: SettingPage) -> SettingPage {
    let hidden_header_style = div()
        .h(px(0.0))
        .max_h(px(0.0))
        .p_0()
        .border_0()
        .overflow_hidden()
        .style()
        .clone();
    page.header_style(&hidden_header_style)
}

fn application_page(
    shell: Entity<Shell>,
    model: SettingsPageViewModel,
    inputs: SettingsPageInputs,
    core_service: CoreServiceSnapshot,
    palette: ShellPalette,
) -> SettingPage {
    let settings = model.settings;

    let mut startup_items = vec![
        app_switch_item(
            "恢复上次窗口",
            "记录窗口大小、位置和最大化状态。",
            settings.restore_window,
            SettingsBoolField::RestoreWindow,
            shell.clone(),
            palette,
        ),
        app_switch_item(
            "启动后自动拉起核心",
            "应用启动后自动启动 mihomo 核心。",
            settings.start_core_after_launch,
            SettingsBoolField::StartCoreAfterLaunch,
            shell.clone(),
            palette,
        ),
    ];
    if cfg!(target_os = "windows") {
        startup_items.push(core_service_switch_item(core_service, shell.clone()));
    }
    startup_items.extend([
        app_switch_item(
            "开机自启",
            "系统登录后自动启动 Air。",
            settings.autostart,
            SettingsBoolField::Autostart,
            shell.clone(),
            palette,
        ),
        app_switch_item(
            "静默启动",
            "软件启动时直接隐藏到托盘。",
            settings.silent_start,
            SettingsBoolField::SilentStartup,
            shell.clone(),
            palette,
        ),
        app_switch_item(
            "隐藏到托盘",
            "点击窗口关闭按钮时隐藏主窗口，不退出应用。",
            settings.close_window_behavior == CloseWindowBehavior::Tray,
            SettingsBoolField::HideToTray,
            shell.clone(),
            palette,
        ),
    ]);

    hide_setting_page_header(SettingPage::new(UnifiedSettingsPage::Application.title()))
        .icon(ComponentIcon::new(
            UnifiedSettingsPage::Application.sidebar_icon(),
        ))
        .description(UnifiedSettingsPage::Application.description())
        .resettable(false)
        .default_open(true)
        .groups(vec![
            SettingGroup::new().title("外观").items(vec![
                theme_choice_item(settings.theme, shell.clone(), palette),
                readonly_value_item("语言", AppLanguage::ZhCn.label(), Icon::Languages, palette),
            ]),
            SettingGroup::new().title("启动").items(startup_items),
            SettingGroup::new()
                .title("网络")
                .items(vec![app_switch_item(
                    "同步系统代理",
                    "启动核心后将 macOS 系统 HTTP/HTTPS/SOCKS 代理指向 Air，停止或关闭时恢复。",
                    settings.system_proxy_enabled,
                    SettingsBoolField::SystemProxy,
                    shell.clone(),
                    palette,
                )]),
            SettingGroup::new().title("其他").items(vec![app_input_item(
                "测速地址",
                inputs.proxy_delay_test_url,
                "代理页单节点和代理组测速时使用的 HTTP 探测地址。",
                palette,
            )]),
        ])
}

fn app_input_item(
    label: &'static str,
    input: Entity<InputState>,
    description: &'static str,
    _palette: ShellPalette,
) -> SettingItem {
    SettingItem::new(
        label,
        SettingField::render(move |_, _, _| Input::new(&input).w_full()),
    )
    .layout(Axis::Vertical)
    .description(description)
}

fn core_service_switch_item(service: CoreServiceSnapshot, shell: Entity<Shell>) -> SettingItem {
    SettingItem::new(
        "内核服务",
        SettingField::switch(
            move |_| service.installed,
            move |value, cx| {
                shell.update(cx, |shell, cx| {
                    shell.request_core_service_toggle(value);
                    cx.notify();
                });
            },
        ),
    )
    .description("通过 Windows Service 启动需要管理员权限的 TUN 内核，避免每次启动核心都弹 UAC。")
}

fn core_page(
    shell: Entity<Shell>,
    model: config_editor::ConfigEditorViewModel,
    inputs: config_editor::ConfigEditorInputs,
    palette: ShellPalette,
) -> SettingPage {
    let form = model.draft.global.clone();
    hide_setting_page_header(SettingPage::new(UnifiedSettingsPage::Core.title()))
        .icon(ComponentIcon::new(UnifiedSettingsPage::Core.sidebar_icon()))
        .resettable(false)
        .groups(with_config_notice_group(
            ConfigEditorGroup::Global,
            &model,
            palette,
            shell.clone(),
            vec![
                SettingGroup::new().title("代理端口").items(vec![
                    input_item(
                        "HTTP(S) 代理端口",
                        inputs.global_port,
                        "HTTP(S) 代理监听端口。",
                        palette,
                    ),
                    input_item(
                        "SOCKS 代理端口",
                        inputs.global_socks_port,
                        "SOCKS4、SOCKS4a、SOCKS5 代理监听端口。",
                        palette,
                    ),
                    input_item(
                        "混合端口",
                        inputs.global_mixed_port,
                        "同时支持 HTTP(S) 和 SOCKS5 协议的代理端口。",
                        palette,
                    ),
                    input_item(
                        "Redirect 透明代理端口",
                        inputs.global_redir_port,
                        "仅 Linux(Android) 和 macOS 适用，只代理 TCP 流量。",
                        palette,
                    ),
                    input_item(
                        "TProxy 透明代理端口",
                        inputs.global_tproxy_port,
                        "仅 Linux(Android) 适用，可代理 TCP 与 UDP 流量。",
                        palette,
                    ),
                ]),
                SettingGroup::new().title("局域网连接").items(vec![
                    config_switch_item(
                        "允许局域网",
                        "允许其他设备通过代理端口访问互联网。",
                        form.allow_lan,
                        true,
                        ConfigBoolField::GlobalAllowLan,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "绑定地址",
                        inputs.global_bind_address,
                        "绑定所有 IP 时填写 *，也可填写单个 IPv4 / IPv6 地址。",
                        palette,
                    ),
                    textarea_item(
                        "允许连接 IP 段",
                        inputs.global_lan_allowed_ips,
                        "仅允许局域网访问开启时生效，每行一个值。",
                        palette,
                    ),
                    textarea_item(
                        "禁止连接 IP 段",
                        inputs.global_lan_disallowed_ips,
                        "黑名单优先级高于白名单，每行一个值。",
                        palette,
                    ),
                    textarea_item(
                        "代理用户验证",
                        inputs.global_authentication,
                        "HTTP(S)、SOCKS、Mixed 代理的用户验证，每行一个值。",
                        palette,
                    ),
                    textarea_item(
                        "跳过验证 IP 段",
                        inputs.global_skip_auth_prefixes,
                        "允许跳过验证的 IP 段，每行一个值。",
                        palette,
                    ),
                ]),
                SettingGroup::new().title("运行").items(vec![
                    config_choice_item(
                        "运行模式",
                        "规则匹配 / 全局代理 / 全局直连。",
                        form.mode.as_str(),
                        "rule",
                        vec![
                            ("rule", "规则", Icon::ListChecks),
                            ("global", "全局", Icon::Globe),
                            ("direct", "直连", Icon::CircleOff),
                        ],
                        ConfigTextField::GlobalMode,
                        shell.clone(),
                        palette,
                    ),
                    config_dropdown_item(
                        "日志级别",
                        "Clash 内核输出到控制台和控制页面的日志等级。",
                        form.log_level.as_str(),
                        "info",
                        vec![
                            ("silent", "静默"),
                            ("error", "错误"),
                            ("warning", "警告"),
                            ("info", "信息"),
                            ("debug", "调试"),
                        ],
                        ConfigTextField::GlobalLogLevel,
                        shell.clone(),
                    ),
                    config_switch_item(
                        "IPv6",
                        "是否允许内核接受 IPv6 流量。",
                        form.ipv6,
                        true,
                        ConfigBoolField::GlobalIpv6,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "Keep Alive 间隔",
                        inputs.global_keep_alive_interval,
                        "TCP Keep Alive 包间隔，单位秒。",
                        palette,
                    ),
                    input_item(
                        "Keep Alive 空闲时间",
                        inputs.global_keep_alive_idle,
                        "TCP Keep Alive 最大空闲时间。",
                        palette,
                    ),
                    config_switch_item(
                        "禁用 Keep Alive",
                        "禁用 TCP Keep Alive，Android 上强制为 true。",
                        form.disable_keep_alive,
                        false,
                        ConfigBoolField::GlobalDisableKeepAlive,
                        shell.clone(),
                        palette,
                    ),
                    config_choice_item(
                        "进程匹配模式",
                        "控制是否让 Clash 匹配进程。",
                        form.find_process_mode.as_str(),
                        "strict",
                        vec![
                            ("always", "总是", Icon::ListFilter),
                            ("strict", "严格", Icon::BadgeInfo),
                            ("off", "关闭", Icon::CircleOff),
                        ],
                        ConfigTextField::GlobalFindProcessMode,
                        shell.clone(),
                        palette,
                    ),
                ]),
                SettingGroup::new().title("外部控制").items(vec![
                    input_item(
                        "API 监听地址",
                        inputs.global_controller,
                        "RESTful API 监听地址。",
                        palette,
                    ),
                    textarea_item(
                        "CORS 允许来源",
                        inputs.global_controller_cors_allow_origins,
                        "API CORS 允许来源，每行一个值。",
                        palette,
                    ),
                    config_switch_item(
                        "允许 Private Network",
                        "API CORS 是否允许 Private Network。",
                        form.external_controller_cors_allow_private_network,
                        true,
                        ConfigBoolField::GlobalControllerCorsAllowPrivateNetwork,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "DOH 服务路径",
                        inputs.global_doh_server,
                        "在 RESTful API 端口上开启 DOH 服务器。",
                        palette,
                    ),
                    input_item(
                        if form.secret_label().starts_with("secret") {
                            "API 访问密钥"
                        } else {
                            form.secret_label()
                        },
                        inputs.global_secret,
                        "API 访问密钥；留空保留已有 secret。",
                        palette,
                    ),
                ]),
                SettingGroup::new().title("缓存与连接").items(vec![
                    config_switch_item(
                        "缓存策略组选择",
                        "储存 API 对策略组的选择。",
                        form.store_selected,
                        true,
                        ConfigBoolField::GlobalStoreSelected,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "缓存 Fake-IP 映射",
                        "储存 Fake-IP 映射表。",
                        form.store_fake_ip,
                        true,
                        ConfigBoolField::GlobalStoreFakeIp,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "统一延迟",
                        "计算 RTT 以消除连接握手等带来的延迟差异。",
                        form.unified_delay,
                        true,
                        ConfigBoolField::GlobalUnifiedDelay,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "TCP 并发",
                        "使用 DNS 解析出的所有 IP 并发连接，采用第一个成功连接。",
                        form.tcp_concurrent,
                        true,
                        ConfigBoolField::GlobalTcpConcurrent,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "出站接口",
                        inputs.global_interface_name,
                        "mihomo 流量出站接口。",
                        palette,
                    ),
                    input_item(
                        "路由标记",
                        inputs.global_routing_mark,
                        "Linux 出站连接默认流量标记。",
                        palette,
                    ),
                ]),
                SettingGroup::new().title("GEO 数据").items(vec![
                    config_switch_item(
                        "GEOIP 数据模式",
                        "更改 GeoIP 使用文件，开启时使用 dat。",
                        form.geodata_mode,
                        false,
                        ConfigBoolField::GlobalGeodataMode,
                        shell.clone(),
                        palette,
                    ),
                    config_choice_item(
                        "GEO 文件加载模式",
                        "GEO 文件加载器。",
                        form.geodata_loader.as_str(),
                        "memconservative",
                        vec![
                            ("standard", "标准", Icon::Gauge),
                            ("memconservative", "节省内存", Icon::MemoryStick),
                        ],
                        ConfigTextField::GlobalGeodataLoader,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "自动更新 GEO",
                        "自动更新 GEO 数据。",
                        form.geo_auto_update,
                        false,
                        ConfigBoolField::GlobalGeoAutoUpdate,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "GEO 更新间隔",
                        inputs.global_geo_update_interval,
                        "更新间隔，单位小时。",
                        palette,
                    ),
                    input_item(
                        "GeoIP 下载地址",
                        inputs.global_geox_geoip,
                        "自定义 GeoIP 下载地址。",
                        palette,
                    ),
                    input_item(
                        "Geosite 下载地址",
                        inputs.global_geox_geosite,
                        "自定义 Geosite 下载地址。",
                        palette,
                    ),
                    input_item(
                        "MMDB 下载地址",
                        inputs.global_geox_mmdb,
                        "自定义 MMDB 下载地址。",
                        palette,
                    ),
                    input_item(
                        "ASN 下载地址",
                        inputs.global_geox_asn,
                        "自定义 ASN 下载地址。",
                        palette,
                    ),
                    input_item(
                        "全局 User-Agent",
                        inputs.global_ua,
                        "自定义外部资源下载时使用的 User-Agent。",
                        palette,
                    ),
                ]),
            ],
        ))
}

fn tun_page(
    shell: Entity<Shell>,
    model: config_editor::ConfigEditorViewModel,
    inputs: config_editor::ConfigEditorInputs,
    palette: ShellPalette,
) -> SettingPage {
    let form = model.draft.tun.clone();
    hide_setting_page_header(SettingPage::new(UnifiedSettingsPage::Tun.title()))
        .icon(ComponentIcon::new(UnifiedSettingsPage::Tun.sidebar_icon()))
        .description(UnifiedSettingsPage::Tun.description())
        .resettable(false)
        .groups(with_config_notice_group(
            ConfigEditorGroup::Tun,
            &model,
            palette,
            shell.clone(),
            vec![
                SettingGroup::new().title("基础").items(vec![
                    config_switch_item(
                        "启用 TUN",
                        "启用 TUN 入站。",
                        form.enable,
                        false,
                        ConfigBoolField::TunEnable,
                        shell.clone(),
                        palette,
                    ),
                    config_choice_item(
                        "协议栈",
                        "TUN 模式堆栈；如无使用问题，建议使用 mixed。",
                        form.stack.as_str(),
                        "gvisor",
                        vec![
                            ("system", "System", Icon::MonitorCog),
                            ("gvisor", "gVisor", Icon::Network),
                            ("mixed", "Mixed", Icon::Layers),
                        ],
                        ConfigTextField::TunStack,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "网卡名称",
                        inputs.tun_device,
                        "指定 TUN 网卡名称；MacOS 只能使用 utun 开头的网卡名。",
                        palette,
                    ),
                    input_item("MTU", inputs.tun_mtu, "最大传输单元。", palette),
                    textarea_item(
                        "DNS 劫持地址",
                        inputs.tun_dns_hijack,
                        "将匹配到的 DNS 连接导入内部 DNS 模块，每行一个值。",
                        palette,
                    ),
                    config_switch_item(
                        "自动路由",
                        "自动设置全局路由，将全局流量路由进入 TUN 网卡。",
                        form.auto_route,
                        false,
                        ConfigBoolField::TunAutoRoute,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "自动重定向",
                        "仅支持 Linux，自动配置 iptables/nftables 以重定向 TCP 连接，需要启用自动路由。",
                        form.auto_redirect,
                        false,
                        ConfigBoolField::TunAutoRedirect,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "自动检测接口",
                        "自动选择流量出口接口；多出口网卡设备建议手动指定出口网卡。",
                        form.auto_detect_interface,
                        false,
                        ConfigBoolField::TunAutoDetectInterface,
                        shell.clone(),
                        palette,
                    ),
                    config_switch_item(
                        "严格路由",
                        "启用自动路由时执行严格路由规则，降低地址和 DNS 泄漏风险。",
                        form.strict_route,
                        false,
                        ConfigBoolField::TunStrictRoute,
                        shell.clone(),
                        palette,
                    ),
                ]),
                SettingGroup::new().title("性能与地址").items(vec![
                    config_switch_item(
                        "GSO",
                        "启用通用分段卸载，仅支持 Linux。",
                        form.gso,
                        false,
                        ConfigBoolField::TunGso,
                        shell.clone(),
                        palette,
                    ),
                    input_item(
                        "GSO 最大长度",
                        inputs.tun_gso_max_size,
                        "数据块的最大长度。",
                        palette,
                    ),
                    input_item(
                        "IPv6 地址",
                        inputs.tun_inet6_address,
                        "指定 TUN 的 IPv6 地址，需要顶层 IPv6 同时开启。",
                        palette,
                    ),
                    input_item(
                        "UDP 超时",
                        inputs.tun_udp_timeout,
                        "UDP NAT 过期时间，单位秒。",
                        palette,
                    ),
                    config_switch_item(
                        "端点独立 NAT",
                        "启用独立于端点的 NAT，性能可能会略有下降。",
                        form.endpoint_independent_nat,
                        false,
                        ConfigBoolField::TunEndpointIndependentNat,
                        shell.clone(),
                        palette,
                    ),
                ]),
                SettingGroup::new().title("路由").items(vec![
                    input_item(
                        "iproute2 路由表索引",
                        inputs.tun_iproute2_table_index,
                        "自动路由生成的 iproute2 路由表索引。",
                        palette,
                    ),
                    input_item(
                        "iproute2 规则索引",
                        inputs.tun_iproute2_rule_index,
                        "自动路由生成的 iproute2 规则起始索引。",
                        palette,
                    ),
                    textarea_item(
                        "路由规则集",
                        inputs.tun_route_address_set,
                        "将规则集中的目标 IP CIDR 加入防火墙，不匹配的流量绕过路由；仅支持 Linux。",
                        palette,
                    ),
                    textarea_item(
                        "排除路由规则集",
                        inputs.tun_route_exclude_address_set,
                        "将规则集中的目标 IP CIDR 加入防火墙，匹配的流量绕过路由；仅支持 Linux。",
                        palette,
                    ),
                    textarea_item(
                        "路由地址",
                        inputs.tun_route_address,
                        "启用自动路由时路由自定义网段，而不是默认路由，每行一个值。",
                        palette,
                    ),
                    textarea_item(
                        "排除路由地址",
                        inputs.tun_route_exclude,
                        "启用自动路由时排除自定义网段，每行一个值。",
                        palette,
                    ),
                ]),
                SettingGroup::new().title("接口").items(vec![
                    textarea_item(
                        "包含接口",
                        inputs.tun_include_interface,
                        "限制被路由的接口，与排除接口冲突，每行一个值。",
                        palette,
                    ),
                    textarea_item(
                        "排除接口",
                        inputs.tun_exclude_interface,
                        "排除被路由的接口，与包含接口冲突，每行一个值。",
                        palette,
                    ),
                ]),
            ],
        ))
}
