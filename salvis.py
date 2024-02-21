import asyncio
from playwright.async_api import async_playwright
import shutil, json, os, base64, sys
from PIL import Image
import lxml.etree as ET, io
# from bs4 import BeautifulSoup


async def createPDF(nameBook: str, book_num: str):
    # Создание цельного PDF из страниц
    # Открываем все изображения во временной папке
    # Сортировка необходима, чтобы избежать артефактов файловой системы
    images = [
        Image.open(f'{book_num}_book_pages/' + img)
        for img in sorted(os.listdir(f'{book_num}_book_pages'), key=len)
        ]

    if os.path.exists('All-Books') is False:
        os.mkdir('All-Books')

    # Сохраняем
    images[0].save(
        f'All-Books/{nameBook}.pdf', "PDF", resolution=100.0, save_all=True, append_images=images[1:]
    )

    # Чистим папку с времянкой
    shutil.rmtree(f'{book_num}_book_pages')


async def authCookies():

    # Проверка куки файла
    if os.path.exists('cookies.json'):

        # Если куки есть подгружаем их и проверяем еще раз
        with open("cookies.json", "r") as f:
            cookies = json.loads(f.read())
        await context.add_cookies(cookies)
        auth_page = await context.new_page()
        await auth_page.goto('https://znanium.ru/user/my-profile')

        # Проверку прошли, то возвращаемся
        if auth_page.url == 'https://znanium.ru/user/my-profile':
            await auth_page.close()
            return
        # Если редирект на логин, то авторизуемся заново
        elif auth_page.url == 'https://znanium.ru/site/login':
            await auth_page.get_by_label('Логин или Email').fill(LOGIN)
            await auth_page.get_by_label('Пароль').fill(PASSWORD)
            await auth_page.get_by_role("button", name="Вход").click()
            await auth_page.wait_for_url('https://znanium.ru/user/my-profile')
            await auth_page.close()
            return

    else:
        auth_page = await context.new_page()
        await auth_page.goto('https://znanium.ru/user/my-profile')

        if auth_page.url == 'https://znanium.ru/site/login':
            await auth_page.get_by_label('Логин или Email').fill(LOGIN)
            await auth_page.get_by_label('Пароль').fill(PASSWORD)
            await auth_page.get_by_role("button", name="Вход").click()
            await auth_page.wait_for_url('https://znanium.ru/user/my-profile')
            await auth_page.close()

            with open("cookies.json", "w") as f:
                f.write(json.dumps(await context.cookies()))


async def splice(book_num, page_num):
    # Склеивание кусочков страницы в одно целое
    # Парсим пути к изображениям и Открываем изображения
    images = [
        Image.open(f'{book_num}_book_pages/tmp{page_num}_img/' + img)
        for img in os.listdir(f'{book_num}_book_pages/tmp{page_num}_img')
        ]

    # Получаем размеры всех изображений
    widths, heights = zip(*(img.size for img in images))

    # Вычисляем общую ширину и высоту для нового изображения и создаем его
    with Image.new('RGB', (max(widths), sum(heights))) as final_img:
        # Вставляем все изображения вертикально
        y_offset = 0
        for img in images:
            final_img.paste(img, (0, y_offset))
            y_offset += img.size[1]

        # Для увеличения резкости, больше 2 не имеет смысла ставить
        # from PIL import ImageEnhance
        # enhancer = ImageEnhance.Sharpness(final_img)
        # enhancer.enhance(2)

        # Сохраняем объединенное изображение
        final_img.save(f'{book_num}_book_pages/page_{page_num}.png')
    # Чистим папку со временными изображениями
    shutil.rmtree(f'{book_num}_book_pages/tmp{page_num}_img')


async def intercept_response(response):
    # Фильтруем нужный нам ответ от сайта
    if response.request.resource_type == "xhr":     # По типу данных
        if 'pgnum' in response.url:                 # По символам в ссылке

            print(response.url)
            book_num = str(response.url).split('=')[1][:-6]
            # Вытаскиваем из ссылки номер загружаемой страницы
            page_num = str(response.url).split('&')[1].replace('pgnum=', '')

            # Это проверка на лоха, для случайно загруженных ранее страниц
            # чтобы не создавались фантомные папки 'tmp{page_num}_img'
            if os.path.exists(f'{book_num}_book_pages/page_{page_num}.png'):
                return

            os.makedirs(f'{book_num}_book_pages/tmp{page_num}_img')            # Создаем временную папку

            # Что bs4, что ET.three работают практически одинаково
            # Мне больше понравился последний
            # 0.15 mls
            # body = str(await response.body(), encoding='utf-8')
            # soup = BeautifulSoup(body, 'lxml-xml')
            # chunks = soup.find('bookpages').findChildren()

            # Парсим из XML кусочки страницы и сохраняем во временную папку
            # https://stackoverflow.com/questions/57871841/fastest-lxml-interface-for-parsing-small-rigidly-structured-xml-files
            context = ET.iterparse(
                io.BytesIO(await response.body())
                )

            for action, chunks in context:
                if "slice" in chunks.tag:
                    string_base64 = str(chunks.text)[22:]   # Отрезаем вначале "data:image/png;base64,"
                    n = str(chunks.tag)[5:]                 # Номер чанка нужен для корректной склейки потом

                    with open(f'{book_num}_book_pages/tmp{page_num}_img/{n}.png', 'wb') as file:
                        file.write(base64.b64decode(string_base64))

            await splice(book_num, page_num)


async def loadingPages(link: str):
    parcerPage = await context.new_page()

    # При загрузке страницы перехватывает ответы
    parcerPage.on("response", intercept_response)
    # Дожидаемся полной загрузки страницы (не факт)
    await parcerPage.goto(link, wait_until='load')
    # Узнаем кол-во страниц в книге
    totalPages = await parcerPage.locator("""#body-root > div.controls > div > div >
                                          div.controls__control-panel.control-panel.flex >
                                          div.control-panel__pages.pages.flex > p""").inner_text()

    # Подставляем порядковый номер страницы ждем ENTER
    # При загрузке книги сразу отдает первую и последнюю страницу
    for i in range(0, int(totalPages[5:])):

        await parcerPage.locator('#page').wait_for(state='visible')
        await parcerPage.locator('#page').clear(force=True)
        await parcerPage.locator('#page').fill(str(i), force=True)
        await parcerPage.locator('#page').press("Enter")

        # Останавливаем цикл до появления ответа от сайта (задержка случайная)
        try:
            async with parcerPage.expect_response(
                        lambda response: '&pgnum=' in response.url
                        and response.status == 200,
                        timeout=7000
                                            ) as response_info:
                response = await response_info.value
                if response.ok:
                    continue
        except:
            continue

    # Узнаем название книги, для корректного имени после сохранения
    nameBook = await parcerPage.locator('#body-root > div.header > div > div > div > div > p > a').all_inner_texts()

    # Ожидание для корректного завершения работ всех функций
    await parcerPage.wait_for_timeout(7000)
    await parcerPage.close()

    # Создаем книгу
    await createPDF(nameBook[0], link.split('=')[1])


async def main() -> None:
    # Начало начал, ебал я рот в корутине переменные тасовать
    global context

    # Используем playwright как самую быструю библиотеку для работы
    # Если я додумаюсь как Bearer вертеть на пальце, то перепишу
    async with async_playwright() as pw:
        browser = await pw.chromium.launch(headless=True) # Можно поменять на firefox или edge
        context = await browser.new_context() # Создаем сессию с куками блекджеком и шлюхами
        save_page = await context.new_page() # Пустая страница, чтобы браузер при закрытии рабочих вкладок не закрылся совсем
        await authCookies() # Логинимся или подгружаем куки

        with open('links.txt', 'r') as f:
            links = [line.strip() for line in f if line.strip()]

        for link in links:
            await loadingPages(link) # singleBook_tab() # 1 вкладка = 1 книга

        await save_page.close()
        await context.close()
        await browser.close()


if __name__ == "__main__":
    global LOGIN
    global PASSWORD

    LOGIN = sys.argv[1]
    PASSWORD = sys.argv[2]

    asyncio.run(main())
