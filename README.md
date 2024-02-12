# Znanium.ru downloader (Page images into a single PDF)
## Ideological Predecessor - [znanium-savebooks](https://github.com/luxname/znanium-savebooks "znanium-savebooks")

Works on Windows, other platforms have not been tested.

The script allows you to download books from the site [znanium](https://znanium.ru/).
For these purposes the **playwright** module is used.

For downloading you need to:
1. Have an account with the purchased book.
2. In the directory with the script create a text file - **links.txt**. In it add links to books.
3. Install all dependencies via `pip install requirements.txt`

Example of program execution from terminal:

`$ python salvis.py login password`

The first argument is **login** from the account \
Second argument - **password**

------------------------------------------
## Идейный предшественник - [znanium-savebooks](https://github.com/luxname/znanium-savebooks "znanium-savebooks")

Работает на Windows, остальные платформы не тестировались.

Скрипт позволяет скачивать книги с сайта [znanium](https://znanium.ru/).
Для данных целей используется модуль **playwright**.

Для скачивания вам необходимо:
1. Иметь аккаунт с купленной книгой.
2. В директории со скриптом создать текстовый файл - **links.txt**. В него добавить ссылки на книги.
3. Через `pip install requirements.txt` - установить все зависимости.

Пример выполнения программы из терминала:

`$ python salvis.py login password`

Первый аргумент - **логин** от учетной записи \
Второй аргумент - **пароль**
