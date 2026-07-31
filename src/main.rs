// salvis — загрузчик книг с znanium.ru.
//
// Архитектура получения данных не менялась: сырой ответ XHR `/read/page` —
// намеренно повреждённые данные, поэтому мы ждём, пока JS читалки сам
// расшифрует страницу и вставит корректный <svg> в `#bookreadcont{N}`, и
// забираем оттуда outerHTML.
//
// Что изменилось в сборке PDF (и почему):
//   - Убран printpdf::Svg::parse. Внутри он вызывает svg2pdf, а потом
//     пересобирает готовый PDF в свой XObject, записывая в его /Resources
//     ТОЛЬКО ColorSpace. svg2pdf же кладёт каждую изолированную группу
//     (clip-path / mask / opacity / blend-mode) в отдельный Form XObject, а
//     прозрачность — в ExtGState. После пересборки операторы `Do` и `gs`
//     ссылаются на имена, которых в словаре ресурсов больше нет, поэтому
//     соответствующее содержимое просто не рисуется. В книге читалки в такие
//     группы завёрнут весь текстовый слой — отсюда «страницы без текста,
//     только какие-то формы и очертания» (уцелело лишь то, что лежало прямо
//     в корневом потоке, без обёрток).
//   - Убрано и второе следствие того же места: printpdf::Svg::parse жёстко
//     задаёт svg2pdf dpi = 300, из-за чего готовый XObject получается
//     размером 72/300 = 0.24 от натуральной величины, а размер страницы мы
//     брали из viewBox как есть (1 единица = 1 pt). Отсюда «масштаб 1/10» —
//     содержимое занимало ~24% ширины листа.
//   - Теперь svg2pdf::to_chunk вызывается напрямую, а страница собирается
//     через pdf-writer вместе с полным chunk'ом ресурсов. usvg::Options.dpi
//     выставлен в 72.0, поэтому пользовательская единица SVG равна точке PDF
//     независимо от того, записан размер как `595`, `595pt` или `210mm`.
//
// Всё, что касается запуска Firefox, install_browsers, new_context(),
// авторизации и Locator API — оставлено без изменений.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use pdf_writer::{Chunk, Content, Finish, Name, Pdf, Rect, Ref};
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

    /// Начать с указанной страницы (по умолчанию — с первой)
    #[arg(long, value_name = "N", default_value_t = 1)]
    from: u32,

    /// Ограничить число выгружаемых страниц — для быстрой проверки результата
    #[arg(long, value_name = "N")]
    pages: Option<u32>,

    /// Дополнительно класть рядом с каждой страницей её исходный .svg
    #[arg(long)]
    dump_svg: bool,

    /// Не собирать общий PDF книги, ограничиться постраничными файлами
    #[arg(long)]
    no_merge: bool,

    /// Запустить Firefox в headless-режиме
    #[arg(long)]
    headless: bool,
}

const COOKIES_PATH: &str = "cookies.json";
const LINKS_PATH: &str = "links.txt";
const PROFILE_URL: &str = "https://znanium.ru/user/my-profile";
const LOGIN_URL: &str = "https://znanium.ru/site/login";
/// Каталог с постраничными файлами (по подкаталогу на книгу)
const PAGES_DIR: &str = "Pages";
/// Каталог с собранными книгами
const BOOKS_DIR: &str = "All-Books";
/// Имя, под которым SVG-страница попадает в /Resources /XObject
const SVG_NAME: Name<'static> = Name(b"S1");

/// Результат извлечения страницы из DOM.
#[derive(serde::Deserialize)]
struct ExtractedSvg {
    /// Самодостаточный SVG: исходная страница плюс вклеенные в неё определения.
    svg: String,
    /// Сколько определений пришлось подтянуть из документа.
    added: u32,
    /// Первые несколько id, которые не удалось найти нигде в документе.
    missing: Vec<String>,
    /// Полное число ненайденных id.
    #[serde(rename = "missingCount")]
    missing_count: u32,
}

/// Забирает страницу из DOM вместе со всем, на что она ссылается.
///
/// Просто `el.outerHTML` брать нельзя: читалка держит глифы шрифтов в общем
/// хранилище за пределами самого `<svg>` страницы, а `<use xlink:href="#id">`
/// в браузере разрешается по всему документу. В выгруженном по отдельности
/// `<svg>` такие ссылки повисают, и весь набранный этими глифами текст
/// исчезает — остаются только те фигуры, что нарисованы прямо в потоке
/// (линейки, плашки, часть контуров). Замерено на реальной странице:
/// 201 уникальная ссылка `<use>` против ровно одного `id` в самом файле.
///
/// Поэтому клонируем узел, транзитивно собираем все ссылки — и `href`, и
/// `url(#...)` в любых атрибутах — и переносим найденные определения в
/// собственный `<defs>` клона.
const EXTRACT_SVG_JS: &str = r##"(el) => {
  const SVG_NS = 'http://www.w3.org/2000/svg';
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
          const re = /url\(\s*['"]?#([^)'"\s]+)/g;
          let m;
          while ((m = re.exec(val)) !== null) out.push(m[1]);
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
  let added = 0;
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
    added++;
    have.add(id);
    if (copy.id) have.add(copy.id);
    for (const node of copy.querySelectorAll('[id]')) have.add(node.id);
    for (const ref of refsOf(copy)) queue.push(ref);
  }

  return {
    svg: clone.outerHTML,
    added: added,
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
        if let Err(err) = process_book(&context, link, &args, &svg_options).await {
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

/// Загружает одну книгу: листает страницы читалки, дожидается отрисовки каждой
/// страницы в DOM, забирает готовый <svg> из `#bookreadcont{N}` и сразу же
/// пишет её на диск отдельным PDF — результат видно, не дожидаясь всей книги.
async fn process_book(
    context: &playwright_rs::protocol::BrowserContext,
    link: &str,
    args: &Args,
    svg_options: &usvg::Options<'_>,
) -> Result<()> {
    println!("Открытие новой вкладки для загрузки книги...");
    let page = context.new_page().await?;

    let book_id = link
        .split('=')
        .nth(1)
        .ok_or_else(|| anyhow!("не удалось извлечь id книги из ссылки: {link}"))?
        .to_string();

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
    let total_pages: u32 = total_pages_text
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0);

    println!("Общее количество страниц: {}", total_pages);
    if total_pages == 0 {
        return Err(anyhow!("не удалось определить число страниц для {link}"));
    }

    // Название нужно до начала обхода: по нему именуется каталог, в который
    // страницы складываются по мере получения.
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

    let safe_title = sanitize_file_name(&book_name);
    let page_dir = PathBuf::from(PAGES_DIR).join(&safe_title);
    fs::create_dir_all(&page_dir)
        .with_context(|| format!("не удалось создать каталог {}", page_dir.display()))?;
    println!("Постраничные файлы: {}", page_dir.display());

    let first = args.from.max(1);
    let last = match args.pages {
        Some(limit) => total_pages.min(first.saturating_add(limit).saturating_sub(1)),
        None => total_pages,
    };
    if first > total_pages {
        return Err(anyhow!(
            "--from {first} больше, чем страниц в книге ({total_pages})"
        ));
    }
    if first != 1 || last != total_pages {
        println!("Диапазон выгрузки: страницы {first}..{last}");
    }

    let mut rendered: Vec<RenderedPage> = Vec::new();

    // Контейнеры #bookreadcontN нумеруются с 1, атрибут data-pagenum совпадает.
    for n in first..=last {
        println!("Запрос страницы {}...", n);
        let input = page.locator("#page");
        input.wait_for(None).await.ok();
        input.clear(None).await.ok();
        input.fill(&n.to_string(), None).await.ok();
        input.press("Enter", None).await.ok();

        let svg_loc = page.locator(&format!("#bookreadcont{n} svg"));

        match tokio::time::timeout(Duration::from_secs(15), svg_loc.wait_for(None)).await {
            Ok(Ok(())) => {}
            _ => {
                eprintln!(
                    "Предупреждение: страница {n} книги {book_id} не отрисовалась за 15с, пропуск"
                );
                continue;
            }
        }

        let extracted: ExtractedSvg = match svg_loc.evaluate(EXTRACT_SVG_JS, None::<()>).await {
            Ok(value) => value,
            Err(err) => {
                eprintln!("Предупреждение: не удалось извлечь SVG страницы {n}: {err:#}");
                continue;
            }
        };
        let svg_html = extracted.svg;
        println!(
            "Страница {} получена ({} байт SVG, вклеено определений: {}).",
            n,
            svg_html.len(),
            extracted.added
        );
        if extracted.missing_count > 0 {
            eprintln!(
                "Предупреждение: на странице {n} осталось {} неразрешённых ссылок (например: {}). \
                 Эта часть содержимого не попадёт в PDF.",
                extracted.missing_count,
                extracted.missing.join(", ")
            );
        }

        if args.dump_svg {
            let svg_path = page_dir.join(format!("page_{n:04}.svg"));
            if let Err(err) = fs::write(&svg_path, &svg_html) {
                eprintln!(
                    "Предупреждение: не удалось записать {}: {err}",
                    svg_path.display()
                );
            }
        }

        // Конвертация и запись отдельного PDF сразу же, а не в конце книги.
        match render_svg(&svg_html, svg_options) {
            Ok(rendered_page) => {
                let pdf_path = page_dir.join(format!("page_{n:04}.pdf"));
                let bytes = assemble_pdf(std::slice::from_ref(&rendered_page));
                match fs::write(&pdf_path, &bytes) {
                    Ok(()) => println!(
                        "  -> {} ({:.1}x{:.1} pt, {} байт PDF)",
                        pdf_path.display(),
                        rendered_page.width_pt,
                        rendered_page.height_pt,
                        bytes.len()
                    ),
                    Err(err) => {
                        eprintln!(
                            "Предупреждение: не удалось записать {}: {err}",
                            pdf_path.display()
                        )
                    }
                }
                if !args.no_merge {
                    rendered.push(rendered_page);
                }
            }
            Err(err) => {
                eprintln!("Предупреждение: страница {n} не сконвертировалась в PDF: {err:#}");
            }
        }
    }

    println!("Закрытие вкладки...");
    page.close().await?;

    if args.no_merge {
        println!("Сборка общего PDF пропущена (--no-merge).");
        return Ok(());
    }
    if rendered.is_empty() {
        return Err(anyhow!("ни одна страница книги {book_id} не была получена"));
    }

    println!("Начало формирования PDF из {} страниц...", rendered.len());
    let book_file = if first == 1 && last == total_pages {
        format!("{safe_title}.pdf")
    } else {
        format!("{safe_title} (стр. {first}-{last}).pdf")
    };
    let out_path = PathBuf::from(BOOKS_DIR).join(book_file);
    write_pdf(&out_path, &rendered)?;
    println!("PDF успешно создан: {}", out_path.display());

    Ok(())
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
fn assemble_pdf(pages: &[RenderedPage]) -> Vec<u8> {
    let mut alloc = Ref::new(1);
    let catalog_id = alloc.bump();
    let page_tree_id = alloc.bump();

    // Идентификаторы страниц нужны заранее: дерево страниц пишется до них.
    let page_slots: Vec<(Ref, Ref)> = pages.iter().map(|_| (alloc.bump(), alloc.bump())).collect();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.pages(page_tree_id)
        .kids(page_slots.iter().map(|(page_id, _)| *page_id))
        .count(page_slots.len() as i32);

    for (rendered, (page_id, content_id)) in pages.iter().zip(page_slots) {
        // Каждый chunk пронумерован с единицы, поэтому перед вклейкой в общий
        // документ его объекты переносятся в сквозную нумерацию.
        let mut map = HashMap::new();
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
fn write_pdf(path: &Path, pages: &[RenderedPage]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("не удалось создать каталог {}", parent.display()))?;
    }
    println!("Запись файла PDF: {}", path.display());
    fs::write(path, assemble_pdf(pages))
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
