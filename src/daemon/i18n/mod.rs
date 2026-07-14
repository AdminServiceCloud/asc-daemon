//! Translations for user-facing CLI output (EN default + RU).
//!
//! Every message shown to the user goes through [`t`] / [`tf`] with a [`Msg`]
//! key, so adding a language means extending one match. Debug and log
//! messages are English-only and bypass this module entirely.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// CLI output language. Stored in config.toml as the `language` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Ru,
}

impl FromStr for Lang {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "en" => Ok(Lang::En),
            "ru" => Ok(Lang::Ru),
            other => Err(format!("unknown language '{other}', expected 'en' or 'ru'")),
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Lang::En => "en",
            Lang::Ru => "ru",
        })
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Set the process-wide output language (normally once at startup, from config).
pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// Current output language.
pub fn lang() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Ru,
        _ => Lang::En,
    }
}

/// Keys of all translatable CLI messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    ServiceInstalled,
    ServiceUninstalled,
    ServiceStarted,
    ServiceStopped,
    ServiceRestarted,
    StatusService,
    StateActive,
    StateInactive,
    StateNotInstalled,
    ConfigLangCurrent,
    ConfigLangSet,
    AppNotFound,
    AppStarted,
    AppAlreadyRunning,
    AppStopped,
    AppNotRunning,
    AppRestarted,
    AppRemoved,
    AppRemoveNeedsYes,
    AppListEmpty,
    AppNameAmbiguous,
    AttachHint,
    AttachDockerOnly,
    AttachStartFirst,
    PkgInstalled,
    PkgAlreadyInstalled,
    PkgNotFound,
    PkgAmbiguous,
    PkgNotInSource,
    PkgPickSource,
    PkgNotAStack,
    PkgStackNoApp,
    PkgStackInstalled,
    PkgStackAppSkipped,
    PkgStartHint,
    PkgPromptName,
    PkgNameInvalid,
    PkgNameTaken,
    PkgNameForSingleApp,
    PkgPolicyDockerOnly,
    PkgUpgraded,
    PkgUpToDate,
    PkgUpgradeStopFirst,
    PkgAuthRequired,
    PkgLicenseRequired,
    PkgLicenseNotice,
    PkgLicensePrompt,
    PkgLicenseAutoAccepted,
    AppLowResources,
    AppStartRiskPrompt,
    AppStartDeclined,
    AuthPromptConfigure,
    AuthPromptToken,
    AuthPromptKey,
    AuthNoKeys,
    AuthSaved,
    AuthRetrying,
    AuthEmpty,
    AuthRemoved,
    AuthNeedMethod,
    AuthInvalidChoice,
    SourceAdded,
    SourceRemoved,
    SourcesEmpty,
    SourceSystemNeedsRoot,
    UpdateSourceDone,
    UpdateDone,
    SearchNoResults,
    ErrGitNotFound,
    OwnerLabel,
    StatusApps,
    StatusCpu,
    StatusMemory,
    StatusDisk,
    NoLogs,
    ErrRootRequired,
    ErrNoSystemd,
    ErrUnsupportedOs,
    ErrDockerNotFound,
    ErrDockerUnreachable,
    ErrImagePull,
    ErrCurlNotFound,
    UpdSettingsHeader,
    UpdSettingLanguage,
    UpdSettingAuto,
    UpdSettingChannel,
    UpdSettingSchedule,
    UpdSettingDir,
    UpdConfirmDefaults,
    UpdAborted,
    UpdPromptLanguage,
    UpdPromptAuto,
    UpdPromptChannel,
    UpdPromptSchedule,
    UpdPromptDir,
    WordEnabled,
    WordDisabled,
    UpdDownloading,
    UpdInstalled,
    UpdUpToDate,
    UpdUpdated,
    UpdRolledBack,
    UpdNoPrevious,
    UpdAutoEnabled,
    UpdAutoDisabled,
    UpdChannelSet,
    UpdStatusInstalled,
    UpdStatusAvailable,
    UpdNoBuildForPlatform,
    UpdNotInstalled,
    SettingsNone,
    SettingsHeader,
    SettingsPromptSelect,
    SettingsPromptValue,
    SettingsSaved,
    SettingsRestartHint,
    ErrSettingNumber,
    ErrSettingRange,
    ErrSettingBool,
    ErrSettingEnum,
    ErrSettingLength,
    ErrQuotaSize,
    ErrQuotaCpu,
}

/// Translate a message key using the current language.
pub fn t(msg: Msg) -> &'static str {
    let (en, ru) = match msg {
        Msg::ServiceInstalled => (
            "Service installed and enabled (systemd unit 'asc'). Start it with: asc service start",
            "Сервис установлен и добавлен в автозапуск (systemd-юнит 'asc'). Запуск: asc service start",
        ),
        Msg::ServiceUninstalled => (
            "Service stopped, disabled and removed",
            "Сервис остановлен, убран из автозапуска и удалён",
        ),
        Msg::ServiceStarted => ("Service started", "Сервис запущен"),
        Msg::ServiceStopped => ("Service stopped", "Сервис остановлен"),
        Msg::ServiceRestarted => ("Service restarted", "Сервис перезапущен"),
        Msg::StatusService => ("Service: {}", "Сервис: {}"),
        Msg::StateActive => ("running", "работает"),
        Msg::StateInactive => ("stopped", "остановлен"),
        Msg::StateNotInstalled => (
            "not installed (run: asc service install)",
            "не установлен (выполните: asc service install)",
        ),
        Msg::ConfigLangCurrent => ("Current language: {}", "Текущий язык: {}"),
        Msg::ConfigLangSet => ("Language set to {}", "Язык переключён на {}"),
        Msg::AppNotFound => ("app '{}' not found", "приложение '{}' не найдено"),
        Msg::AppStarted => ("App '{}' started", "Приложение '{}' запущено"),
        Msg::AppAlreadyRunning => (
            "App '{}' is already running",
            "Приложение '{}' уже запущено",
        ),
        Msg::AppStopped => ("App '{}' stopped", "Приложение '{}' остановлено"),
        Msg::AppNotRunning => (
            "App '{}' is not running",
            "Приложение '{}' и так остановлено",
        ),
        Msg::AppRestarted => ("App '{}' restarted", "Приложение '{}' перезапущено"),
        Msg::AttachHint => (
            "Attached to '{}' — press Ctrl+C to detach (the app keeps running)",
            "Подключено к '{}' — Ctrl+C для отключения (приложение продолжит работать)",
        ),
        Msg::AttachDockerOnly => (
            "attach is available for Docker apps only ('{}' is a {} app)",
            "attach доступен только для Docker-приложений ('{}' — приложение типа {})",
        ),
        Msg::AttachStartFirst => (
            "app '{}' is not running — start it first: asc app start {}",
            "приложение '{}' не запущено — сначала запустите: asc app start {}",
        ),
        Msg::AppRemoved => ("App '{}' removed", "Приложение '{}' удалено"),
        Msg::AppRemoveNeedsYes => (
            "removing app '{}' will delete all its data; re-run with --yes to confirm",
            "удаление приложения '{}' сотрёт все его данные; для подтверждения повторите команду с --yes",
        ),
        Msg::AppListEmpty => ("No apps installed", "Приложения не установлены"),
        Msg::AppNameAmbiguous => (
            "several apps are named '{}' — use the app id instead",
            "названию '{}' соответствует несколько приложений — используйте id",
        ),
        Msg::PkgInstalled => ("Installed '{}' version {}", "Установлено '{}' версии {}"),
        Msg::PkgAlreadyInstalled => (
            "app '{}' is already installed",
            "приложение '{}' уже установлено",
        ),
        Msg::PkgNotFound => (
            "package '{}' not found in any registry (refresh indexes: asc update)",
            "пакет '{}' не найден ни в одном реестре (обновить индексы: asc update)",
        ),
        Msg::PkgAmbiguous => (
            "package '{}' is available from several sources: {} — re-run with --source <name>",
            "пакет '{}' доступен из нескольких источников: {} — повторите с --source <имя>",
        ),
        Msg::PkgNotInSource => (
            "package '{}' not found in source '{}'",
            "пакет '{}' не найден в источнике '{}'",
        ),
        Msg::PkgPickSource => (
            "Package '{}' is available from several sources — pick one:",
            "Пакет '{}' доступен из нескольких источников — выберите один:",
        ),
        Msg::PkgNotAStack => (
            "package '{}' is not a stack — install it as a whole: asc install {}",
            "пакет '{}' — не стек, устанавливается целиком: asc install {}",
        ),
        Msg::PkgStackNoApp => (
            "stack '{}' has no app '{}'",
            "в стеке '{}' нет приложения '{}'",
        ),
        Msg::PkgStackInstalled => (
            "Stack '{}' installed ({} apps)",
            "Стек '{}' установлен (приложений: {})",
        ),
        Msg::PkgStackAppSkipped => (
            "App '{}' is already installed — skipped",
            "Приложение '{}' уже установлено — пропущено",
        ),
        Msg::PkgStartHint => ("Start it: asc app start {}", "Запуск: asc app start {}"),
        Msg::PkgPromptName => (
            "App name [{}] — Enter keeps the default: ",
            "Название приложения [{}] — Enter оставит по умолчанию: ",
        ),
        Msg::PkgNameInvalid => (
            "invalid app name '{}': 1-64 printable characters, no leading/trailing spaces",
            "недопустимое название '{}': 1-64 печатных символа, без пробелов по краям",
        ),
        Msg::PkgNameTaken => (
            "name '{}' is already used by another app",
            "название '{}' уже занято другим приложением",
        ),
        Msg::PkgNameForSingleApp => (
            "a custom name applies to a single app, not the whole '{}' stack",
            "пользовательское название применимо к одному приложению, а не ко всему стеку '{}'",
        ),
        Msg::PkgUpgraded => (
            "App '{}' upgraded: {} → {}",
            "Приложение '{}' обновлено: {} → {}",
        ),
        Msg::PkgUpToDate => (
            "App '{}' is already up to date ({})",
            "Приложение '{}' уже актуально ({})",
        ),
        Msg::PkgUpgradeStopFirst => (
            "app '{}' is running — stop it before upgrading: asc app stop {}",
            "приложение '{}' запущено — перед обновлением остановите его: asc app stop {}",
        ),
        Msg::PkgAuthRequired => (
            "repository {} looks private and no authorization is configured — set it up: asc auth add <host> --token <token> (https) or asc auth add <host> --ssh-key (git@/ssh)",
            "репозиторий {} похож на приватный, авторизация не настроена — настройте: asc auth add <хост> --token <токен> (https) или asc auth add <хост> --ssh-key (git@/ssh)",
        ),
        Msg::PkgLicenseRequired => (
            "installing '{}' from source '{}' ({}) requires accepting the repository license",
            "установка '{}' из источника '{}' ({}) требует принятия лицензии репозитория",
        ),
        Msg::PkgLicenseNotice => (
            "Installing '{}' from registry source '{}' — repository {}. The repository license:",
            "Установка '{}' из источника реестра '{}' — репозиторий {}. Лицензия репозитория:",
        ),
        Msg::PkgLicensePrompt => ("Accept the license? [y/N] ", "Принимаете лицензию? [y/N] "),
        Msg::PkgLicenseAutoAccepted => (
            "License accepted automatically (non-interactive input); the full text is in the repository's LICENSE file",
            "Лицензия принята автоматически (неинтерактивный ввод); полный текст — в файле LICENSE репозитория",
        ),
        Msg::AppLowResources => (
            "warning: the host may not have enough resources for '{}': {}",
            "внимание: хосту может не хватить ресурсов для '{}': {}",
        ),
        Msg::AppStartRiskPrompt => (
            "Start anyway at your own risk? [y/N] ",
            "Всё равно запустить на свой страх и риск? [y/N] ",
        ),
        Msg::AppStartDeclined => ("start of '{}' cancelled", "запуск '{}' отменён"),
        Msg::AuthPromptConfigure => (
            "Repository {} looks private. Configure authorization for '{}' now? [y/N] ",
            "Репозиторий {} похож на приватный. Настроить авторизацию для '{}' сейчас? [y/N] ",
        ),
        Msg::AuthPromptToken => (
            "Access token for '{}' (stored in {}): ",
            "Токен доступа для '{}' (будет сохранён в {}): ",
        ),
        Msg::AuthPromptKey => ("Select an SSH key for '{}':", "Выберите SSH-ключ для '{}':"),
        Msg::AuthNoKeys => (
            "no private SSH keys found in ~/.ssh — add a key or use an https URL with a token",
            "в ~/.ssh не найдено приватных SSH-ключей — добавьте ключ или используйте https-URL с токеном",
        ),
        Msg::AuthSaved => (
            "Authorization for '{}' saved: {}",
            "Авторизация для '{}' сохранена: {}",
        ),
        Msg::AuthRetrying => (
            "Retrying with the saved authorization…",
            "Повторяю с сохранённой авторизацией…",
        ),
        Msg::AuthEmpty => (
            "no git credentials configured",
            "учётные данные git не настроены",
        ),
        Msg::AuthRemoved => (
            "Credentials for '{}' removed",
            "Учётные данные для '{}' удалены",
        ),
        Msg::AuthNeedMethod => (
            "specify --token <token> or --ssh-key [path]",
            "укажите --token <токен> или --ssh-key [путь]",
        ),
        Msg::AuthInvalidChoice => ("invalid choice", "неверный выбор"),
        Msg::PkgPolicyDockerOnly => (
            "package '{}' is not a Docker app: this server allows regular users to install Docker apps only (root setting: [policy] user_install)",
            "пакет '{}' — не Docker-приложение: на этом сервере обычным пользователям разрешена установка только Docker-приложений (настройка root: [policy] user_install)",
        ),
        Msg::SourceAdded => ("Source '{}' added", "Источник '{}' добавлен"),
        Msg::SourceRemoved => ("Source '{}' removed", "Источник '{}' удалён"),
        Msg::SourcesEmpty => ("No sources configured", "Источники не настроены"),
        Msg::SourceSystemNeedsRoot => (
            "source '{}' is a system source (managed by root; run with sudo)",
            "источник '{}' — системный (управляется root; выполните через sudo)",
        ),
        Msg::UpdateSourceDone => (
            "  {}: {} packages indexed",
            "  {}: заиндексировано пакетов — {}",
        ),
        Msg::UpdateDone => (
            "Registry indexes updated: {} sources, {} packages",
            "Индексы реестров обновлены: источников — {}, пакетов — {}",
        ),
        Msg::SearchNoResults => (
            "nothing found for '{}'",
            "по запросу '{}' ничего не найдено",
        ),
        Msg::ErrGitNotFound => (
            "git not found: install git to use the package manager",
            "git не найден: установите git для работы пакетного менеджера",
        ),
        Msg::OwnerLabel => ("Owner: {}", "Владелец: {}"),
        Msg::StatusApps => (
            "Apps: {} running / {} total",
            "Приложения: запущено {} из {}",
        ),
        Msg::StatusCpu => (
            "CPU: {} ({} cores), load average {}",
            "CPU: {} ({} ядер), load average {}",
        ),
        Msg::StatusMemory => ("Memory: {}", "Память: {}"),
        Msg::StatusDisk => ("Disk {}: {}", "Диск {}: {}"),
        Msg::NoLogs => ("no logs yet", "логов пока нет"),
        Msg::ErrRootRequired => (
            "this command requires root privileges (run with sudo)",
            "команда требует прав root (запустите через sudo)",
        ),
        Msg::ErrNoSystemd => (
            "systemd not found: only systemd-based distributions are supported for now",
            "systemd не найден: пока поддерживаются только дистрибутивы на systemd",
        ),
        Msg::ErrUnsupportedOs => (
            "service management is only supported on Linux for now",
            "управление сервисом пока поддерживается только на Linux",
        ),
        Msg::ErrCurlNotFound => (
            "curl not found: install curl to download updates and registry indexes",
            "curl не найден: установите curl для загрузки обновлений и индексов реестров",
        ),
        Msg::ErrDockerNotFound => (
            "Docker is not installed — container apps need it; install: curl -fsSL https://get.docker.com | sh",
            "Docker не установлен — он нужен контейнерным приложениям; установка: curl -fsSL https://get.docker.com | sh",
        ),
        Msg::ErrImagePull => (
            "cannot pull image '{}' from its registry (check the image name and registry access)",
            "не удаётся скачать образ '{}' из реестра (проверьте имя образа и доступ к реестру)",
        ),
        Msg::ErrDockerUnreachable => (
            "cannot reach Docker at {} (is Docker running? set the socket path in [docker] socket)",
            "не удаётся подключиться к Docker по {} (Docker запущен? путь к сокету — [docker] socket)",
        ),
        Msg::UpdSettingsHeader => (
            "ASC installation settings (defaults):",
            "Настройки установки ASC (по умолчанию):",
        ),
        Msg::UpdSettingLanguage => ("  Language:           {}", "  Язык:                 {}"),
        Msg::UpdSettingAuto => ("  Auto-updates:       {}", "  Автообновления:       {}"),
        Msg::UpdSettingChannel => ("  Update channel:     {}", "  Канал обновлений:     {}"),
        Msg::UpdSettingSchedule => (
            "  Check schedule:     daily {}",
            "  Расписание проверки:  ежедневно {}",
        ),
        Msg::UpdSettingDir => ("  Install directory:  {}", "  Каталог установки:    {}"),
        Msg::UpdConfirmDefaults => (
            "Install with these settings? [Y/n/c(hange)]",
            "Установить с этими настройками? [Y/n/c — изменить]",
        ),
        Msg::UpdAborted => ("installation aborted", "установка прервана"),
        Msg::UpdPromptLanguage => ("Language (en/ru)", "Язык (en/ru)"),
        Msg::UpdPromptAuto => ("Auto-updates (on/off)", "Автообновления (on/off)"),
        Msg::UpdPromptChannel => (
            "Update channel (stable/beta)",
            "Канал обновлений (stable/beta)",
        ),
        Msg::UpdPromptSchedule => (
            "Daily check time (HH:MM)",
            "Время ежедневной проверки (HH:MM)",
        ),
        Msg::UpdPromptDir => ("Install directory", "Каталог установки"),
        Msg::WordEnabled => ("enabled", "включены"),
        Msg::WordDisabled => ("disabled", "выключены"),
        Msg::UpdDownloading => ("Downloading {}...", "Скачивание {}..."),
        Msg::UpdInstalled => (
            "Daemon {} installed. Check it: asc status",
            "Демон {} установлен. Проверка: asc status",
        ),
        Msg::UpdUpToDate => (
            "Already up to date ({})",
            "Уже установлена последняя версия ({})",
        ),
        Msg::UpdUpdated => ("Updated to {}", "Обновлено до {}"),
        Msg::UpdRolledBack => (
            "Rolled back to the previous version",
            "Выполнен откат на предыдущую версию",
        ),
        Msg::UpdNoPrevious => (
            "no previous version to roll back to",
            "нет предыдущей версии для отката",
        ),
        Msg::UpdAutoEnabled => ("Auto-updates enabled", "Автообновления включены"),
        Msg::UpdAutoDisabled => (
            "Auto-updates disabled (update manually: asc-updater update)",
            "Автообновления выключены (обновление вручную: asc-updater update)",
        ),
        Msg::UpdChannelSet => ("Update channel set to {}", "Канал обновлений: {}"),
        Msg::UpdStatusInstalled => ("Installed: {}", "Установлено: {}"),
        Msg::UpdStatusAvailable => ("Available ({}): {}", "Доступно ({}): {}"),
        Msg::UpdNoBuildForPlatform => (
            "release {} has no build for this platform ({})",
            "в релизе {} нет сборки под эту платформу ({})",
        ),
        Msg::UpdNotInstalled => (
            "daemon is not installed yet (run: asc-updater install)",
            "демон ещё не установлен (выполните: asc-updater install)",
        ),
        Msg::SettingsNone => (
            "app '{}' has no configurable settings (no asc.settings.yaml in the package)",
            "у приложения '{}' нет настраиваемых параметров (в пакете нет asc.settings.yaml)",
        ),
        Msg::SettingsHeader => ("Settings of '{}':", "Настройки приложения '{}':"),
        Msg::SettingsPromptSelect => (
            "Setting number to change (Enter — done): ",
            "Номер настройки для изменения (Enter — готово): ",
        ),
        Msg::SettingsPromptValue => ("New value for '{}'{}: ", "Новое значение '{}'{}: "),
        Msg::SettingsSaved => ("Setting '{}' = {} saved", "Настройка '{}' = {} сохранена"),
        Msg::SettingsRestartHint => (
            "Restart the app to apply the changes: asc app restart {}",
            "Перезапустите приложение, чтобы применить изменения: asc app restart {}",
        ),
        Msg::ErrSettingNumber => ("value must be a number", "значение должно быть числом"),
        Msg::ErrSettingRange => (
            "value must be in range {}",
            "значение должно быть в диапазоне {}",
        ),
        Msg::ErrSettingBool => (
            "value must be a boolean: true/false (yes/no, on/off)",
            "значение должно быть логическим: true/false (yes/no, on/off)",
        ),
        Msg::ErrSettingEnum => (
            "value must be one of: {}",
            "значение должно быть одним из: {}",
        ),
        Msg::ErrSettingLength => (
            "value length must be in range {}",
            "длина значения должна быть в диапазоне {}",
        ),
        Msg::ErrQuotaSize => (
            "invalid size '{}' in quota (expected e.g. 512M, 2G, 1T)",
            "неверный размер '{}' в квоте (ожидается, например, 512M, 2G, 1T)",
        ),
        Msg::ErrQuotaCpu => (
            "quota max_cpu must be a positive number of cores",
            "квота max_cpu должна быть положительным числом ядер",
        ),
    };
    match lang() {
        Lang::En => en,
        Lang::Ru => ru,
    }
}

/// Translate a message key and substitute `{}` with `value`.
pub fn tf(msg: Msg, value: impl fmt::Display) -> String {
    t(msg).replacen("{}", &value.to_string(), 1)
}

/// Translate a message key and substitute two `{}` placeholders in order.
pub fn tf2(msg: Msg, a: impl fmt::Display, b: impl fmt::Display) -> String {
    t(msg)
        .replacen("{}", &a.to_string(), 1)
        .replacen("{}", &b.to_string(), 1)
}

/// Translate a message key and substitute three `{}` placeholders in order.
pub fn tf3(msg: Msg, a: impl fmt::Display, b: impl fmt::Display, c: impl fmt::Display) -> String {
    t(msg)
        .replacen("{}", &a.to_string(), 1)
        .replacen("{}", &b.to_string(), 1)
        .replacen("{}", &c.to_string(), 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lang_parses_case_insensitively() {
        assert_eq!("EN".parse::<Lang>().unwrap(), Lang::En);
        assert_eq!("ru".parse::<Lang>().unwrap(), Lang::Ru);
        assert!("de".parse::<Lang>().is_err());
    }

    #[test]
    fn tf_substitutes_placeholder() {
        set_lang(Lang::En);
        assert_eq!(tf(Msg::ConfigLangSet, Lang::Ru), "Language set to ru");
    }
}
