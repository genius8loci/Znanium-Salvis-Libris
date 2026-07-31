// salvis — загрузчик книг с znanium.ru.
//
// Смена архитектуры по сравнению с вашей последней рабочей версией:
//   - Убран перехват XHR `/read/page` и вся логика parse_slices/splice_slices.
//     Доказано (см. переписку): сырой ответ этого эндпоинта — намеренно
//     повреждённые данные (base64-картинка меняется при повторном запросе той
//     же страницы; 97%+ ссылок <use> не совпадают со своими <defs>). Парсить
//     его бессмысленно при любом количестве повторных попыток.
//   - Вместо этого читалка сама расшифровывает данные и вставляет готовый,
//     корректный <svg> внутрь `#bookreadcont{N}` в DOM. Мы просто дожидаемся
//     этого и забираем `outerHTML` (подтверждено сравнением: DOM-экспорт даёт
//     97.8-98.2% совпадения <use>/<defs>, у сырого XHR — 1.5%).
//   - create_pdf собирал PDF из PNG через старый API printpdf (кортеж
//     PdfDocument::new/add_page/get_layer). Новый build_pdf вставляет SVG
//     напрямую как векторный XObject (Svg::parse + add_xobject +
//     Op::UseXobject) — без растеризации, текст остаётся текстом.
//
// Всё, что касается запуска Firefox, headless(false), install_browsers,
// new_context(), avторизации и Locator API — оставлено без изменений
// один в один с вашим файлом, который реально скомпилировался и запустился.
// Это подтверждено вашим компилятором, а не документацией — трогать не стал.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use playwright_rs::{
    api::LaunchOptions,
    install_browsers,
    protocol::{Cookie, Page, Playwright},
};
use printpdf::{Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Svg, XObjectTransform};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Аргументы командной строки: логин и пароль от аккаунта znanium.ru
#[derive(Parser, Debug)]
#[command(name = "salvis", about = "Znanium.ru downloader")]
struct Args {
    /// Логин от учётной записи
    login: String,
    /// Пароль от учётной записи
    password: String,
}

const COOKIES_PATH: &str = "cookies.json";
const LINKS_PATH: &str = "links.txt";
const PROFILE_URL: &str = "https://znanium.ru/user/my-profile";
const LOGIN_URL: &str = "https://znanium.ru/site/login";

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

    println!("Запуск Firefox (headless = false)...");
    let browser = playwright
        .firefox()
        // Выключение headless-режима через переменные окружения Playwright
        .launch_with_options(LaunchOptions::new().headless(false))
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

    println!("Чтение ссылок из файла {}...", LINKS_PATH);
    let links = read_links(LINKS_PATH)?;
    println!("Найдено ссылок для загрузки: {}", links.len());

    for (i, link) in links.iter().enumerate() {
        println!(">>> Обработка книги {} из {}: {}", i + 1, links.len(), link);
        if let Err(err) = process_book(&context, link).await {
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
/// Без изменений — этот блок уже подтверждён вашей рабочей сборкой.
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
/// Без изменений.
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

/// Сохраняет актуальные cookies контекста в файл. Без изменений.
async fn save_cookies(context: &playwright_rs::protocol::BrowserContext) -> Result<()> {
    println!("Сохранение новой сессии в {}...", COOKIES_PATH);
    let cookies = context.cookies(None).await?;
    let json = serde_json::to_string(&cookies)?;
    fs::write(COOKIES_PATH, json)?;
    Ok(())
}

/// Загружает одну книгу: листает страницы читалки, дожидается отрисовки
/// каждой страницы в DOM и забирает готовый <svg> из #bookreadcont{N}.
async fn process_book(context: &playwright_rs::protocol::BrowserContext, link: &str) -> Result<()> {
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

    let mut svg_pages: Vec<String> = Vec::with_capacity(total_pages as usize);

    // Контейнеры #bookreadcontN нумеруются с 1 (подтверждено HTML-снапшотом
    // читалки: bookreadcont1..bookreadcontN, атрибут data-pagenum совпадает).
    for n in 1..=total_pages {
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

        let eval_result: Result<String, _> =
            svg_loc.evaluate("(el) => el.outerHTML", None::<()>).await;
        match eval_result {
            Ok(svg_html) => {
                println!("Страница {} получена ({} байт SVG).", n, svg_html.len());
                svg_pages.push(svg_html);
            }
            Err(err) => {
                eprintln!("Предупреждение: не удалось извлечь SVG страницы {n}: {err:#}");
            }
        }
    }

    println!("Извлечение названия книги...");
    let book_name = page
        .locator("#body-root > div.header > div > div > div > div > p > a")
        .text_content()
        .await?
        .unwrap_or_else(|| format!("book_{book_id}"));
    println!("Название: {}", book_name);

    println!("Закрытие вкладки...");
    page.close().await?;

    if svg_pages.is_empty() {
        return Err(anyhow!("ни одна страница книги {book_id} не была получена"));
    }

    println!("Начало формирования PDF...");
    build_pdf(&book_name, &svg_pages)?;

    Ok(())
}

/// Собирает список уже расшифрованных браузером SVG-страниц в один векторный
/// PDF — каждая страница как вставленный XObject, без растеризации текста.
fn build_pdf(book_title: &str, svg_pages: &[String]) -> Result<()> {
    let mut doc = PdfDocument::new(book_title);
    let mut warnings = Vec::new();
    let mut pages = Vec::with_capacity(svg_pages.len());

    for (idx, svg_str) in svg_pages.iter().enumerate() {
        if idx % 10 == 0 || idx == svg_pages.len() - 1 {
            println!("Добавление страницы {} в PDF...", idx + 1);
        }

        let (w_pt, h_pt) = extract_viewbox_size(svg_str).unwrap_or((595.32, 841.92)); // запасной размер — A4 в pt

        let xobject = Svg::parse(svg_str, &mut warnings)
            .map_err(|e| anyhow!("не удалось разобрать SVG страницы {}: {e}", idx + 1))?;
        let xobject_id = doc.add_xobject(&xobject);

        let page_mm = |pt: f32| Mm(pt * 25.4 / 72.0); // 1 pt = 25.4/72 мм
        let ops = vec![Op::UseXobject {
            id: xobject_id,
            transform: XObjectTransform::default(),
        }];
        pages.push(PdfPage::new(page_mm(w_pt), page_mm(h_pt), ops));
    }

    doc.with_pages(pages);
    let pdf_bytes = doc.save(&PdfSaveOptions::default(), &mut warnings);

    fs::create_dir_all("All-Books")?;
    let safe_title = book_title
        .chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect::<String>();
    let out_path = PathBuf::from("All-Books").join(format!("{safe_title}.pdf"));
    println!("Запись файла PDF: {:?}", out_path);
    fs::write(&out_path, pdf_bytes)?;

    if !warnings.is_empty() {
        eprintln!(
            "Предупреждения printpdf для «{book_title}»: {} шт.",
            warnings.len()
        );
    }

    println!("PDF успешно создан.");
    Ok(())
}

/// Извлекает ширину/высоту из атрибута viewBox="minX minY width height" (в pt).
fn extract_viewbox_size(svg: &str) -> Option<(f32, f32)> {
    let start = svg.find("viewBox=\"")? + "viewBox=\"".len();
    let end = svg[start..].find('"')? + start;
    let parts: Vec<f32> = svg[start..end]
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();
    if parts.len() == 4 {
        Some((parts[2], parts[3]))
    } else {
        None
    }
}
