// salvis — загрузчик книг с znanium.ru.
//
// Получение данных: сырой ответ XHR `/read/page` — намеренно повреждённые
// данные, поэтому мы ждём, пока JS читалки сам расшифрует страницу и вставит
// корректный <svg> в `#bookreadcont{N}`, и забираем результат из DOM.
//
// Два места, где страница теряла содержимое, и как это решено:
//
//   1. Глифы шрифтов читалка держит в общем хранилище ЗА ПРЕДЕЛАМИ <svg>
//      страницы. В браузере `<use xlink:href="#font_1_1_1">` разрешается по
//      всему документу, поэтому на экране всё правильно, но в вырезанном
//      `el.outerHTML` такие ссылки повисают: на реальной странице это 201
//      уникальная ссылка `<use>` против ровно одного `id` внутри файла. Весь
//      набранный этими глифами текст исчезал, оставались только фигуры,
//      нарисованные прямо в потоке. Решение — EXTRACT_SVG_JS ниже.
//
//   2. printpdf::Svg::parse (убран) прогонял SVG через svg2pdf, а затем
//      пересобирал результат в свой XObject, записывая в /Resources ТОЛЬКО
//      ColorSpace. svg2pdf же кладёт каждую изолированную группу (clip-path /
//      mask / opacity / blend-mode) в отдельный Form XObject, а прозрачность —
//      в ExtGState; после пересборки операторы `Do` и `gs` ссылались на имена,
//      которых в словаре ресурсов больше нет. Он же жёстко задавал svg2pdf
//      dpi = 300, из-за чего XObject выходил размером 72/300 = 0.24 от
//      натурального, тогда как размер листа брался из viewBox как 1 ед. = 1 pt.
//      Теперь svg2pdf::to_chunk вызывается напрямую, страница собирается через
//      pdf-writer вместе с полным chunk'ом ресурсов, а usvg работает с
//      dpi = 72, поэтому единица SVG равна точке PDF независимо от того,
//      записан размер как `595`, `595pt` или `210mm`.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use pdf_writer::{Chunk, Content, Finish, Name, Pdf, Rect, Ref, TextStr};
use playwright_rs::{
    api::LaunchOptions,
    install_browsers,
    protocol::{Cookie, Page, Playwright},
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use svg2pdf::{ConversionOptions, usvg};

/// Аргументы командной строки: логин и пароль от аккаунта znanium.ru
#[derive(Parser, Debug)]
#[command(name = "salvis", about = "Znanium.ru downloader")]
struct Args {
    /// Логин от учётной записи
    login: String,
    /// Пароль от учётной записи
    password: String,
    /// Запустить Firefox в headless-режиме
    #[arg(long)]
    headless: bool,
}

const COOKIES_PATH: &str = "cookies.json";
const LINKS_PATH: &str = "links.txt";
const PROFILE_URL: &str = "https://znanium.ru/user/my-profile";
const LOGIN_URL: &str = "https://znanium.ru/site/login";
/// Каталог с собранными книгами
const BOOKS_DIR: &str = "All-Books";
/// Каталог с постраничными файлами (по подкаталогу на книгу)
const PAGES_DIR: &str = "Pages";
/// Файл в каталоге книги, по которому запуск узнаёт её без открытия читалки
const MANIFEST_NAME: &str = "book.json";
/// Имя, под которым SVG-страница попадает в /Resources /XObject
const SVG_NAME: Name<'static> = Name(b"S1");
/// Сколько ждать отрисовки страницы читалкой
const PAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// Сколько раз просить одну и ту же страницу, прежде чем сдаться.
/// Молча потерянная страница — это дыра в книге на несколько сотен листов.
const PAGE_ATTEMPTS: u32 = 2;

/// Результат извлечения страницы из DOM.
#[derive(serde::Deserialize)]
struct ExtractedSvg {
    /// Самодостаточный SVG: исходная страница плюс вклеенные в неё определения.
    svg: String,
    /// Первые несколько id, которые не удалось найти нигде в документе.
    missing: Vec<String>,
    /// Полное число ненайденных id.
    #[serde(rename = "missingCount")]
    missing_count: u32,
}

/// Забирает страницу из DOM вместе со всем, на что она ссылается.
///
/// Клонируем узел, транзитивно собираем все ссылки — и `href`, и `url(#...)` в
/// любых атрибутах — и переносим найденные определения в собственный `<defs>`
/// клона, получая самодостаточный SVG (см. пункт 1 в шапке файла).
const EXTRACT_SVG_JS: &str = r##"(el) => {
  const SVG_NS = 'http://www.w3.org/2000/svg';
  const URL_REF = /url\(\s*['"]?#([^)'"\s]+)/g;
  const clone = el.cloneNode(true);

  let defs = null;
  for (const child of clone.children) {
    if (child.tagName.toLowerCase() === 'defs') { defs = child; break; }
  }
  if (!defs) {
    defs = document.createElementNS(SVG_NS, 'defs');
    clone.insertBefore(defs, clone.firstChild);
  }

  const refsOf = (root) => {
    const out = [];
    const stack = [root];
    while (stack.length) {
      const node = stack.pop();
      if (node.attributes) {
        for (const attr of node.attributes) {
          const val = attr.value;
          if (!val) continue;
          if (attr.localName === 'href') {
            if (val.charAt(0) === '#') out.push(val.slice(1));
            continue;
          }
          if (val.indexOf('url(') === -1) continue;
          URL_REF.lastIndex = 0;
          let m;
          while ((m = URL_REF.exec(val)) !== null) out.push(m[1]);
        }
      }
      for (const child of node.children) stack.push(child);
    }
    return out;
  };

  const have = new Set();
  if (clone.id) have.add(clone.id);
  for (const node of clone.querySelectorAll('[id]')) have.add(node.id);

  const seen = new Set();
  const missing = [];
  const queue = refsOf(clone);
  while (queue.length) {
    const id = queue.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    if (have.has(id)) continue;

    const source = document.getElementById(id);
    if (!source) { missing.push(id); continue; }

    const copy = source.cloneNode(true);
    defs.appendChild(copy);
    have.add(id);
    if (copy.id) have.add(copy.id);
    for (const node of copy.querySelectorAll('[id]')) have.add(node.id);
    for (const ref of refsOf(copy)) queue.push(ref);
  }

  return {
    svg: clone.outerHTML,
    missing: missing.slice(0, 5),
    missingCount: missing.length,
  };
}"##;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    println!("Запуск программы. Логин: {}", args.login);

    println!("Проверка и установка браузера Firefox...");
    install_browsers(Some(&["firefox"]))
        .await
        .context("не удалось установить Firefox")?;

    println!("Инициализация Playwright...");
    let playwright = Playwright::launch()
        .await
        .context("не удалось запустить playwright-rs")?;

    println!("Запуск Firefox (headless = {})...", args.headless);
    let browser = playwright
        .firefox()
        .launch_with_options(LaunchOptions::new().headless(args.headless))
        .await
        .context("не удалось запустить Firefox")?;

    println!("Создание контекста браузера...");
    let context = browser
        .new_context()
        .await
        .context("не удалось создать контекст браузера")?;

    // Пустая страница, чтобы браузер не закрылся при закрытии рабочих вкладок
    let _keepalive_page = context.new_page().await?;

    println!("Старт процесса авторизации...");
    auth_cookies(&context, &args.login, &args.password).await?;

    // Системные шрифты подгружаются один раз на весь запуск: они нужны только
    // если в SVG встретится настоящий <text>, но перечитывать их на каждую из
    // сотен страниц было бы непозволительно дорого.
    println!("Подготовка конвертера SVG (загрузка системных шрифтов)...");
    let svg_options = build_usvg_options();

    println!("Чтение ссылок из файла {}...", LINKS_PATH);
    let links = read_links(LINKS_PATH)?;
    println!("Найдено ссылок для загрузки: {}", links.len());

    for (i, link) in links.iter().enumerate() {
        println!(">>> Обработка книги {} из {}: {}", i + 1, links.len(), link);
        if let Err(err) = process_book(&context, link, &svg_options).await {
            eprintln!("Ошибка при обработке {link}: {err:#}");
        }
        println!(">>> Завершена работа с книгой: {}", link);
    }

    println!("Закрытие браузера...");
    browser.close().await?;
    println!("Программа успешно завершила работу.");
    Ok(())
}

/// Читает ссылки на книги из текстового файла, пропуская пустые строки.
fn read_links(path: &str) -> Result<Vec<String>> {
    let content =
        fs::read_to_string(path).with_context(|| format!("не удалось прочитать {path}"))?;
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect())
}

/// Проверка и восстановление сессии: подгружает cookies.json либо логинится заново.
async fn auth_cookies(
    context: &playwright_rs::protocol::BrowserContext,
    login: &str,
    password: &str,
) -> Result<()> {
    if let Ok(raw) = fs::read_to_string(COOKIES_PATH) {
        println!(
            "Найден файл {}. Подгрузка сохраненной сессии...",
            COOKIES_PATH
        );
        let cookies: Vec<Cookie> =
            serde_json::from_str(&raw).context("повреждённый cookies.json")?;
        context.add_cookies(&cookies).await?;

        println!(
            "Переход на страницу профиля ({}) для проверки...",
            PROFILE_URL
        );
        let auth_page = context.new_page().await?;
        auth_page.goto(PROFILE_URL, None).await?;

        if auth_page.url() == PROFILE_URL {
            println!("Сессия действительна.");
            auth_page.close().await?;
            return Ok(());
        }

        if auth_page.url() == LOGIN_URL {
            println!("Сессия истекла. Необходима повторная авторизация.");
            perform_login(&auth_page, login, password).await?;
            save_cookies(context).await?;
        }
        auth_page.close().await?;
        return Ok(());
    }

    println!(
        "Файл {} не найден. Выполняется вход по логину/паролю...",
        COOKIES_PATH
    );
    let auth_page = context.new_page().await?;
    auth_page.goto(PROFILE_URL, None).await?;

    if auth_page.url() == LOGIN_URL {
        perform_login(&auth_page, login, password).await?;
        save_cookies(context).await?;
    }
    auth_page.close().await?;
    Ok(())
}

/// Заполняет форму авторизации и дожидается перехода в личный кабинет.
async fn perform_login(page: &Page, login: &str, password: &str) -> Result<()> {
    println!("Заполнение поля 'Логин или Email'...");
    page.get_by_label("Логин или Email", false)
        .fill(login, None)
        .await?;
    println!("Заполнение поля 'Пароль'...");
    page.get_by_label("Пароль", false)
        .fill(password, None)
        .await?;
    println!("Нажатие кнопки 'Вход'...");
    page.get_by_role(
        playwright_rs::protocol::AriaRole::Button,
        Some(playwright_rs::protocol::GetByRoleOptions::default().name("Вход")),
    )
    .click(None)
    .await?;
    println!("Ожидание редиректа в профиль...");
    page.wait_for_url(PROFILE_URL, None).await?;
    println!("Успешная авторизация.");
    Ok(())
}

/// Сохраняет актуальные cookies контекста в файл.
async fn save_cookies(context: &playwright_rs::protocol::BrowserContext) -> Result<()> {
    println!("Сохранение новой сессии в {}...", COOKIES_PATH);
    let cookies = context.cookies(None).await?;
    let json = serde_json::to_string(&cookies)?;
    fs::write(COOKIES_PATH, json)?;
    Ok(())
}

/// Загружает одну книгу: листает читалку, забирает готовый <svg> из
/// `#bookreadcont{N}`, кладёт каждую страницу на диск парой `.svg` + `.pdf`, а
/// в конце собирает из содержимого каталога целую книгу.
///
/// Уже лежащие на диске страницы не перекачиваются, поэтому повторный запуск
/// доливает только пропуски. Если пропусков нет и книга уже собрана, браузер
/// для этой ссылки не открывается вовсе.
async fn process_book(
    context: &playwright_rs::protocol::BrowserContext,
    link: &str,
    svg_options: &usvg::Options<'_>,
) -> Result<()> {
    let book_id = link
        .split('=')
        .nth(1)
        .ok_or_else(|| anyhow!("не удалось извлечь id книги из ссылки: {link}"))?
        .to_string();

    // Быстрый путь: по прошлому запуску известно и число страниц, и что все они
    // на месте — тогда читалка не нужна.
    if let Some(dir) = find_book_dir(&book_id)
        && let Some(manifest) = read_manifest(&dir)
        && missing_pages(&dir, manifest.total_pages).is_empty()
    {
        let out_path = book_pdf_path(&manifest.title);
        if out_path.exists() {
            println!(
                "Все {} страниц на диске, книга уже собрана — пропуск: {}",
                manifest.total_pages,
                out_path.display()
            );
            return Ok(());
        }
        println!(
            "Все {} страниц на диске, собираем книгу без загрузки...",
            manifest.total_pages
        );
        return build_book(&dir, &manifest, HashMap::new(), svg_options);
    }

    println!("Открытие новой вкладки для загрузки книги...");
    let page = context.new_page().await?;

    println!("Переход по ссылке: {}", link);
    page.goto(link, None).await?;

    println!("Ожидание загрузки интерфейса читалки и количества страниц...");
    let total_pages_loc = page.locator(
        "#body-root > div.controls > div > div > \
         div.controls__control-panel.control-panel.flex > \
         div.control-panel__pages.pages.flex > p",
    );
    total_pages_loc.wait_for(None).await.ok();
    let total_pages_text = total_pages_loc.text_content().await?.unwrap_or_default();
    let total_pages = parse_total_pages(&total_pages_text)
        .ok_or_else(|| anyhow!("не удалось определить число страниц для {link}"))?;
    println!("Общее количество страниц: {}", total_pages);

    // Название нужно до обхода: по нему именуется каталог, куда страницы
    // ложатся по мере получения.
    println!("Извлечение названия книги...");
    let book_name = page
        .locator("#body-root > div.header > div > div > div > div > p > a")
        .text_content()
        .await
        .ok()
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("book_{book_id}"));
    println!("Название: {}", book_name);

    // Идентичность книги задаёт id из ссылки, а не название: одноимённые книги
    // не должны затирать друг друга, а переименование на сайте не должно
    // приводить к повторной выкачке.
    let manifest = BookManifest {
        link: link.to_string(),
        title: book_name,
        total_pages,
    };
    let page_dir = find_book_dir(&book_id).unwrap_or_else(|| {
        PathBuf::from(PAGES_DIR).join(format!(
            "({book_id}) {}",
            sanitize_file_name(&manifest.title)
        ))
    });
    fs::create_dir_all(&page_dir)
        .with_context(|| format!("не удалось создать каталог {}", page_dir.display()))?;
    write_manifest(&page_dir, &manifest)?;
    println!("Каталог страниц: {}", page_dir.display());

    let pending = missing_pages(&page_dir, total_pages);
    if pending.len() < total_pages as usize {
        println!(
            "Уже на диске: {} из {}. К загрузке: {}",
            total_pages as usize - pending.len(),
            total_pages,
            pending.len()
        );
    }

    // Поле ввода номера страницы одно на всю книгу, пересоздавать его на каждой
    // итерации незачем; дожидаемся его появления один раз, дальше `fill`
    // сам ждёт готовности элемента.
    let input = page.locator("#page");
    input.wait_for(None).await.ok();

    let mut fresh: HashMap<u32, RenderedPage> = HashMap::with_capacity(pending.len());

    // Контейнеры #bookreadcontN нумеруются с 1, атрибут data-pagenum совпадает.
    for n in pending {
        println!("Запрос страницы {}...", n);
        let svg_loc = page.locator(format!("#bookreadcont{n} svg"));
        let mut extracted: Option<ExtractedSvg> = None;

        for attempt in 1..=PAGE_ATTEMPTS {
            input.clear(None).await.ok();
            input.fill(&n.to_string(), None).await.ok();
            input.press("Enter", None).await.ok();

            if !matches!(
                tokio::time::timeout(PAGE_TIMEOUT, svg_loc.wait_for(None)).await,
                Ok(Ok(()))
            ) {
                eprintln!(
                    "Предупреждение: страница {n} не отрисовалась за {}с (попытка {attempt} из {PAGE_ATTEMPTS})",
                    PAGE_TIMEOUT.as_secs()
                );
                continue;
            }

            match svg_loc.evaluate(EXTRACT_SVG_JS, None::<()>).await {
                Ok(value) => {
                    extracted = Some(value);
                    break;
                }
                Err(err) => eprintln!(
                    "Предупреждение: не удалось извлечь SVG страницы {n} (попытка {attempt} из {PAGE_ATTEMPTS}): {err:#}"
                ),
            }
        }

        let Some(extracted) = extracted else {
            eprintln!("Страница {n} книги {book_id} пропущена: не удалось получить SVG");
            continue;
        };
        println!(
            "Страница {} получена ({} байт SVG).",
            n,
            extracted.svg.len()
        );

        // Остаток намеренно повреждённых читалкой ссылок: эти глифы не найти
        // нигде в документе, соответствующие символы в PDF не попадут.
        if extracted.missing_count > 0 {
            eprintln!(
                "Предупреждение: на странице {n} осталось {} неразрешённых ссылок (например: {})",
                extracted.missing_count,
                extracted.missing.join(", ")
            );
        }

        let rendered_page = match render_svg(&extracted.svg, svg_options) {
            Ok(rendered_page) => rendered_page,
            Err(err) => {
                eprintln!("Предупреждение: страница {n} не сконвертировалась в PDF: {err:#}");
                continue;
            }
        };

        // Страница ложится на диск сразу: обрыв на 380-й из 411 не должен
        // стоить всей проделанной работы. PDF пишется первым, .svg — вторым,
        // потому что именно наличие пары считается признаком готовой страницы.
        let page_title = format!("{} — с. {n}", manifest.title);
        if let Err(err) = write_pdf(
            &page_pdf_path(&page_dir, n),
            &page_title,
            std::slice::from_ref(&rendered_page),
        ) {
            eprintln!("Предупреждение: {err:#}");
            continue;
        }
        let svg_path = page_svg_path(&page_dir, n);
        if let Err(err) = fs::write(&svg_path, &extracted.svg) {
            eprintln!(
                "Предупреждение: не удалось записать {}: {err}",
                svg_path.display()
            );
            continue;
        }
        fresh.insert(n, rendered_page);
    }

    println!("Закрытие вкладки...");
    page.close().await?;

    build_book(&page_dir, &manifest, fresh, svg_options)
}

/// Собирает книгу целиком из каталога её страниц.
///
/// Страницы, отрисованные в этом запуске, берутся из памяти; всё остальное
/// перечитывается с диска и рендерится заново из сохранённого `.svg` — поэтому
/// докачка и первый запуск дают одинаковый результат.
fn build_book(
    page_dir: &Path,
    manifest: &BookManifest,
    mut fresh: HashMap<u32, RenderedPage>,
    svg_options: &usvg::Options<'_>,
) -> Result<()> {
    println!(
        "Начало формирования PDF из {} страниц...",
        manifest.total_pages
    );
    let mut pages = Vec::with_capacity(manifest.total_pages as usize);
    let mut lost = Vec::new();

    for n in 1..=manifest.total_pages {
        if let Some(rendered_page) = fresh.remove(&n) {
            pages.push(rendered_page);
            continue;
        }
        match fs::read_to_string(page_svg_path(page_dir, n))
            .map_err(anyhow::Error::from)
            .and_then(|svg| render_svg(&svg, svg_options))
        {
            Ok(rendered_page) => pages.push(rendered_page),
            Err(err) => {
                eprintln!("Предупреждение: страница {n} не попала в книгу: {err:#}");
                lost.push(n);
            }
        }
    }

    if pages.is_empty() {
        return Err(anyhow!(
            "ни одной страницы «{}» нет на диске",
            manifest.title
        ));
    }
    if !lost.is_empty() {
        eprintln!(
            "Внимание: в книгу вошло {} страниц из {}; не хватает: {}{}",
            pages.len(),
            manifest.total_pages,
            lost.iter()
                .take(20)
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
            if lost.len() > 20 { " ..." } else { "" }
        );
        eprintln!("Запустите программу ещё раз — недостающие страницы будут докачаны.");
    }

    let out_path = book_pdf_path(&manifest.title);
    write_pdf(&out_path, &manifest.title, &pages)?;
    println!("PDF успешно создан: {}", out_path.display());
    Ok(())
}

/// Что известно о книге между запусками.
///
/// Лежит в каталоге страниц и нужен ровно для одного: понять на старте, сколько
/// у книги страниц, не открывая читалку.
#[derive(serde::Serialize, serde::Deserialize)]
struct BookManifest {
    /// Ссылка, по которой книга выкачивалась
    #[serde(default)]
    link: String,
    title: String,
    total_pages: u32,
}

/// Каталог книги ищется по id из ссылки, а не по названию: id стабилен, а
/// название может измениться на сайте или совпасть у разных книг.
fn find_book_dir(book_id: &str) -> Option<PathBuf> {
    let prefix = format!("({book_id}) ");
    fs::read_dir(PAGES_DIR)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
        })
}

fn read_manifest(page_dir: &Path) -> Option<BookManifest> {
    let raw = fs::read_to_string(page_dir.join(MANIFEST_NAME)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_manifest(page_dir: &Path, manifest: &BookManifest) -> Result<()> {
    let path = page_dir.join(MANIFEST_NAME);
    fs::write(&path, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("не удалось записать {}", path.display()))
}

fn page_pdf_path(page_dir: &Path, n: u32) -> PathBuf {
    page_dir.join(format!("page_{n:04}.pdf"))
}

fn page_svg_path(page_dir: &Path, n: u32) -> PathBuf {
    page_dir.join(format!("page_{n:04}.svg"))
}

fn book_pdf_path(title: &str) -> PathBuf {
    PathBuf::from(BOOKS_DIR).join(format!("{}.pdf", sanitize_file_name(title)))
}

/// Номера страниц, которых на диске ещё нет.
///
/// Страница считается готовой только когда лежат оба файла: `.pdf` — то, что
/// читает человек, `.svg` — то, из чего пересобирается книга. Обрыв записи
/// между ними просто означает, что страницу перекачают.
fn missing_pages(page_dir: &Path, total_pages: u32) -> Vec<u32> {
    (1..=total_pages)
        .filter(|&n| !page_pdf_path(page_dir, n).exists() || !page_svg_path(page_dir, n).exists())
        .collect()
}

/// Вытаскивает число страниц из подписи вида «из 110».
///
/// Берём последнюю группу цифр, а не все цифры подряд: если в подписи окажется
/// и текущая страница («1 из 110»), склейка всех цифр дала бы 1110.
fn parse_total_pages(text: &str) -> Option<u32> {
    text.rsplit(|c: char| !c.is_ascii_digit())
        .find(|part| !part.is_empty())
        .and_then(|digits| digits.parse().ok())
        .filter(|&pages| pages > 0)
}

/// Одна сконвертированная страница: chunk со всеми объектами svg2pdf, ссылка на
/// корневой XObject и размер страницы в точках PDF.
struct RenderedPage {
    chunk: Chunk,
    root: Ref,
    width_pt: f32,
    height_pt: f32,
}

/// Опции usvg, общие на весь запуск.
///
/// `dpi = 72.0` принципиально: usvg приводит размеры к пикселям через dpi, а мы
/// затем трактуем результат как точки PDF. При 72 dpi пиксель равен точке, и
/// страница получает верный физический размер независимо от того, записан ли
/// размер в SVG без единиц, в `pt` или в `mm`.
fn build_usvg_options() -> usvg::Options<'static> {
    let mut options = usvg::Options {
        dpi: 72.0,
        ..usvg::Options::default()
    };
    options.fontdb_mut().load_system_fonts();
    options
}

/// Разбирает SVG страницы и конвертирует его в самодостаточный chunk PDF.
fn render_svg(svg: &str, options: &usvg::Options<'_>) -> Result<RenderedPage> {
    let tree = usvg::Tree::from_str(svg, options).map_err(|err| anyhow!("usvg parse: {err}"))?;
    let size = tree.size();

    let (chunk, root) = svg2pdf::to_chunk(&tree, ConversionOptions::default())
        .map_err(|err| anyhow!("svg2pdf: {err}"))?;

    Ok(RenderedPage {
        chunk,
        root,
        width_pt: size.width(),
        height_pt: size.height(),
    })
}

/// Собирает многостраничный PDF: каждая страница — свой media box и свой
/// корневой XObject из svg2pdf, вместе со всем chunk'ом его ресурсов
/// (вложенные Form XObject'ы, ExtGState, Shading, Pattern, шрифты).
fn assemble_pdf(title: &str, pages: &[RenderedPage]) -> Vec<u8> {
    let mut alloc = Ref::new(1);
    let catalog_id = alloc.bump();
    let page_tree_id = alloc.bump();
    let info_id = alloc.bump();

    // Идентификаторы страниц нужны заранее: дерево страниц пишется до них.
    let page_slots: Vec<(Ref, Ref)> = pages.iter().map(|_| (alloc.bump(), alloc.bump())).collect();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.document_info(info_id)
        .title(TextStr(title))
        .producer(TextStr("salvis"));
    pdf.pages(page_tree_id)
        .kids(page_slots.iter().map(|(page_id, _)| *page_id))
        .count(page_slots.len() as i32);

    let mut map = HashMap::new();
    for (rendered, (page_id, content_id)) in pages.iter().zip(page_slots) {
        // Каждый chunk пронумерован с единицы, поэтому перед вклейкой в общий
        // документ его объекты переносятся в сквозную нумерацию.
        map.clear();
        let chunk = rendered
            .chunk
            .renumber(|old| *map.entry(old).or_insert_with(|| alloc.bump()));
        let root = map[&rendered.root];

        let mut page = pdf.page(page_id);
        page.media_box(Rect::new(0.0, 0.0, rendered.width_pt, rendered.height_pt));
        page.parent(page_tree_id);
        page.contents(content_id);
        let mut resources = page.resources();
        resources.x_objects().pair(SVG_NAME, root);
        resources.finish();
        page.finish();

        // to_chunk отдаёт XObject размером ровно 1x1 pt, растягиваем его на лист.
        let mut content = Content::new();
        content
            .transform([rendered.width_pt, 0.0, 0.0, rendered.height_pt, 0.0, 0.0])
            .x_object(SVG_NAME);
        pdf.stream(content_id, &content.finish());

        pdf.extend(&chunk);
    }

    pdf.finish()
}

/// Пишет собранный PDF на диск, создавая недостающие каталоги.
fn write_pdf(path: &Path, title: &str, pages: &[RenderedPage]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать каталог {}", parent.display()))?;
    }
    fs::write(path, assemble_pdf(title, pages))
        .with_context(|| format!("не удалось записать {}", path.display()))?;
    Ok(())
}

/// Заменяет запрещённые в именах файлов символы и убирает крайние пробелы/точки.
fn sanitize_file_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let trimmed = replaced.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "book".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_pages_reads_the_last_number() {
        assert_eq!(parse_total_pages("110"), Some(110));
        assert_eq!(parse_total_pages("из 411"), Some(411));
        assert_eq!(parse_total_pages("1 из 110"), Some(110));
        assert_eq!(parse_total_pages(""), None);
        assert_eq!(parse_total_pages("из 0"), None);
    }

    #[test]
    fn page_counts_as_done_only_with_both_files() {
        let dir = std::env::temp_dir().join("salvis-missing-pages-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // 1 — обе половины на месте, 2 — только PDF, 3 — только SVG, 4 — ничего.
        fs::write(page_pdf_path(&dir, 1), "pdf").unwrap();
        fs::write(page_svg_path(&dir, 1), "svg").unwrap();
        fs::write(page_pdf_path(&dir, 2), "pdf").unwrap();
        fs::write(page_svg_path(&dir, 3), "svg").unwrap();

        assert_eq!(missing_pages(&dir, 4), vec![2, 3, 4]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn manifest_survives_a_round_trip() {
        let dir = std::env::temp_dir().join("salvis-manifest-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let manifest = BookManifest {
            link: "https://znanium.ru/read?id=84127".to_string(),
            title: "Аудитор, 2015, №1-2".to_string(),
            total_pages: 110,
        };
        write_manifest(&dir, &manifest).unwrap();

        let read = read_manifest(&dir).unwrap();
        assert_eq!(read.link, manifest.link);
        assert_eq!(read.title, manifest.title);
        assert_eq!(read.total_pages, 110);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_name_loses_forbidden_characters() {
        assert_eq!(
            sanitize_file_name("Стратегии: аспекты "),
            "Стратегии_ аспекты"
        );
        assert_eq!(sanitize_file_name("  "), "book");
    }
}
