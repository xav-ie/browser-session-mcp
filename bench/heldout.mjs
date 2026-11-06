// Held-out tasks: the same kinds as bench/tasks.mjs on other pages, written
// after act was developed against that suite, to see how it does on tasks it
// was never tuned on. HELDOUT1 was written once the main work was done;
// HELDOUT2 after the fixes HELDOUT1 prompted. Each was written before act ran
// on it. docs/jev.md reports both. To run them in jev-browser's own harness,
// export one as LOCAL from its bench/tasks.local.mjs.
const TI = "https://the-internet.herokuapp.com";
const W = "https://en.wikipedia.org/wiki/";

export const HELDOUT1 = [
  { id: "ho-ti-logout", cat: "form", url: `${TI}/login`,
    steps: [
      { goal: "Log in", values: { username: "tomsmith", password: "SuperSecretPassword!" }, assert: "() => location.pathname === '/secure'" },
      { goal: "Log out", assert: "() => location.pathname === '/login' && document.body.innerText.includes('You logged out')" },
    ] },
  { id: "ho-ti-dropdown-1", cat: "widget", url: `${TI}/dropdown`,
    steps: [{ goal: "Select Option 1 in the dropdown", assert: "() => document.querySelector('#dropdown').value === '1'" }] },
  { id: "ho-ti-checkboxes-none", cat: "widget", url: `${TI}/checkboxes`,
    steps: [{ goal: "Make both checkboxes unchecked", assert: "() => [...document.querySelectorAll('#checkboxes input')].every(c => !c.checked)" }] },
  { id: "ho-ti-key-tab", cat: "actions", url: `${TI}/key_presses`,
    steps: [{ goal: "Press the Tab key", assert: "() => document.querySelector('#result').innerText === 'You entered: TAB'" }] },
  { id: "ho-books-travel", cat: "navigation", url: "https://books.toscrape.com/",
    steps: [
      { goal: "Open the Travel category", assert: "() => location.pathname.includes('travel')" },
      { goal: "Open the book \"It's Only the Himalayas\"", assert: "() => location.pathname.includes('its-only-the-himalayas')" },
    ] },
  { id: "ho-quotes-author", cat: "navigation", url: "https://quotes.toscrape.com/",
    steps: [{ goal: "Open the about page of the author of the first quote", assert: "() => location.pathname.startsWith('/author/Albert-Einstein')" }] },
  { id: "ho-quotes-login", cat: "form", url: "https://quotes.toscrape.com/login",
    steps: [{ goal: "Log in", values: { username: "jev", password: "pw123" }, assert: "() => document.body.innerText.includes('Logout')" }] },
  { id: "ho-quotes-tag", cat: "navigation", url: "https://quotes.toscrape.com/",
    steps: [{ goal: "Show the quotes tagged love", assert: "() => location.pathname === '/tag/love/'" }] },
  { id: "ho-wiki-search", cat: "large-page", url: "https://en.wikipedia.org/wiki/Main_Page",
    steps: [{ goal: "Search for the article and open it", values: { query: "Ada Lovelace" }, assert: "() => location.pathname === '/wiki/Ada_Lovelace'" }] },
  { id: "ho-wiki-link", cat: "large-page", url: "https://en.wikipedia.org/wiki/Ada_Lovelace",
    steps: [{ goal: "Open the linked article about the Analytical Engine", assert: "() => /^\\/wiki\\/Analytical_[Ee]ngine$/.test(location.pathname)" }] },
  { id: "ho-todomvc-complete", cat: "spa", url: "https://demo.playwright.dev/todomvc/",
    steps: [{ goal: "Add a new todo item, then mark it as completed", values: { text: "read a book" },
      assert: "() => [...document.querySelectorAll('.todo-list li.completed')].map(l => l.innerText.trim()).join('|') === 'read a book'" }] },
  { id: "ho-saucedemo-sort", cat: "e2e", url: "https://www.saucedemo.com/",
    steps: [
      { goal: "Log in", values: { username: "standard_user", password: "secret_sauce" } },
      { goal: "Sort the products by price, low to high", assert: "() => document.querySelector('.inventory_item_price')?.innerText === '$7.99'" },
    ] },
  { id: "ho-webform-select", cat: "form", url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [{ goal: "Choose 'Three' in the dropdown select, then submit the form", assert: "() => new URLSearchParams(location.search).get('my-select') === '3'" }] },
  { id: "ho-github-issues", cat: "large-page", url: "https://github.com/microsoft/playwright",
    steps: [{ goal: "Open the repository's Issues tab", assert: "() => /\\/issues\\/?$/.test(location.pathname)" }] },
  { id: "ho-neg-missing", cat: "negative", expect: "fail", url: "https://quotes.toscrape.com/",
    steps: [{ goal: "Open the site's pricing page", assert: "() => location.pathname.includes('pricing')" }] },
];

export const HELDOUT2 = [
  { id: "h2-ti-status-404", cat: "navigation", url: `${TI}/status_codes`,
    steps: [{ goal: "Open the 404 status code page", assert: "() => location.pathname === '/status_codes/404'" }] },
  { id: "h2-ti-dynamic-1", cat: "dynamic", url: `${TI}/dynamic_loading/1`,
    steps: [{ goal: "Start the loading and wait until the result text appears", assert: "() => getComputedStyle(document.querySelector('#finish')).display !== 'none'" }] },
  { id: "h2-ti-slider", cat: "widget", url: `${TI}/horizontal_slider`,
    steps: [{ goal: "Set the slider to 4", assert: "() => document.querySelector('input[type=range]').value === '4'" }] },
  { id: "h2-books-mystery", cat: "navigation", url: "https://books.toscrape.com/",
    steps: [
      { goal: "Open the Mystery category", assert: "() => location.pathname.includes('mystery')" },
      { goal: "Open the book \"Sharp Objects\"", assert: "() => location.pathname.includes('sharp-objects')" },
    ] },
  { id: "h2-quotes-next", cat: "navigation", url: "https://quotes.toscrape.com/",
    steps: [{ goal: "Go to the next page of quotes", assert: "() => location.pathname === '/page/2/'" }] },
  { id: "h2-quotes-tag-insp", cat: "navigation", url: "https://quotes.toscrape.com/",
    // Scored with the check below fixed afterwards: the run checked for
    // exactly /tag/inspirational/, but the site's tag links add page/1/.
    steps: [{ goal: "Show the quotes tagged inspirational", assert: "() => location.pathname.startsWith('/tag/inspirational/')" }] },
  { id: "h2-wiki-search", cat: "large-page", url: W + "Main_Page",
    steps: [{ goal: "Search for the article and open it", values: { query: "Grace Hopper" }, assert: "() => location.pathname === '/wiki/Grace_Hopper'" }] },
  { id: "h2-wiki-cobol", cat: "large-page", url: W + "Grace_Hopper",
    steps: [{ goal: "Open the linked article about COBOL", assert: "() => location.pathname === '/wiki/COBOL'" }] },
  { id: "h2-wiki-hillary", cat: "large-page", url: W + "Mount_Everest",
    steps: [{ goal: "Open the linked article about Edmund Hillary", assert: "() => location.pathname === '/wiki/Edmund_Hillary'" }] },
  { id: "h2-todomvc-delete", cat: "spa", url: "https://demo.playwright.dev/todomvc/",
    steps: [{ goal: "Add the two todos, then delete 'buy milk'", values: { first: "buy milk", second: "call mom" }, maxActions: 12,
      assert: "() => [...document.querySelectorAll('.todo-list li')].map(l => l.innerText.trim()).join('|') === 'call mom'" }] },
  { id: "h2-saucedemo-cart", cat: "e2e", url: "https://www.saucedemo.com/",
    steps: [{ goal: "Log in, add the Sauce Labs Bike Light to the cart and open the cart", maxActions: 12,
      values: { username: "standard_user", password: "secret_sauce" },
      assert: "() => location.pathname === '/cart.html' && document.body.innerText.includes('Sauce Labs Bike Light')" }] },
  { id: "h2-webform-text", cat: "form", url: "https://www.selenium.dev/selenium/web/web-form.html",
    steps: [{ goal: "Type the text into the text input, then submit the form", values: { text: "hello" },
      assert: "() => location.pathname.includes('submitted') && new URLSearchParams(location.search).get('my-text') === 'hello'" }] },
  { id: "h2-github-actions", cat: "large-page", url: "https://github.com/microsoft/playwright",
    steps: [{ goal: "Open the repository's Actions tab", assert: "() => /\\/actions\\/?$/.test(location.pathname)" }] },
  { id: "h2-hn-new", cat: "navigation", url: "https://news.ycombinator.com/",
    steps: [{ goal: "Open the page of the newest stories", assert: "() => location.pathname === '/newest'" }] },
  { id: "h2-neg-status", cat: "negative", expect: "fail", url: `${TI}/status_codes`,
    steps: [{ goal: "Open the 999 status code page", assert: "() => location.pathname.includes('999')" }] },
];
