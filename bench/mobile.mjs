// Phone tasks, written 2026-11-06 before act ran on any of them. bench/run.mjs
// runs them in a session opened with useMobileUA (a touch phone, 390x844)
// with --mobile, and as a desktop session without, to compare.
const TI = "https://the-internet.herokuapp.com";
const MW = "https://en.m.wikipedia.org/wiki/";
const SWIPER = "https://swiperjs.com/demos";
export const MOBILE = [
  { id: "m-wiki-menu", cat: "touch-nav", url: MW + "Main_Page",
    steps: [{ goal: "Open the main menu", assert: "() => document.getElementById('main-menu-input')?.checked === true" }] },
  { id: "m-wiki-search", cat: "touch-form", url: MW + "Main_Page",
    steps: [{ goal: "Search for the article and open it", values: { query: "Marie Curie" }, assert: "() => location.pathname === '/wiki/Marie_Curie'" }] },
  { id: "m-swiper-next", cat: "carousel", url: `${SWIPER}/010-default/core/index.html`,
    steps: [{ goal: "Go to the next slide", assert: "() => document.querySelector('.swiper').swiper.activeIndex === 1" }] },
  { id: "m-swiper-third", cat: "carousel", url: `${SWIPER}/010-default/core/index.html`,
    steps: [{ goal: "Show slide 3", assert: "() => document.querySelector('.swiper').swiper.activeIndex === 2" }] },
  { id: "m-swiper-dots", cat: "carousel", url: `${SWIPER}/030-pagination/core/index.html`,
    steps: [{ goal: "Show the second slide", assert: "() => document.querySelector('.swiper').swiper.activeIndex === 1" }] },
  { id: "m-todomvc-add", cat: "touch-form", url: "https://demo.playwright.dev/todomvc/",
    steps: [{ goal: "Add a new todo item", values: { text: "water the plants" },
      assert: "() => [...document.querySelectorAll('.todo-list li')].map(l => l.innerText.trim()).join('|') === 'water the plants'" }] },
  { id: "m-saucedemo-menu", cat: "touch-nav", url: "https://www.saucedemo.com/",
    steps: [
      { goal: "Log in", values: { username: "standard_user", password: "secret_sauce" }, assert: "() => location.pathname === '/inventory.html'" },
      { goal: "Open the side menu", assert: "() => document.querySelector('.bm-menu-wrap')?.getAttribute('aria-hidden') === 'false'" },
    ] },
  { id: "m-ti-checkbox", cat: "touch-widget", url: `${TI}/checkboxes`,
    steps: [{ goal: "Check checkbox 1", assert: "() => document.querySelectorAll('#checkboxes input')[0].checked" }] },
  { id: "m-ti-dropdown", cat: "touch-widget", url: `${TI}/dropdown`,
    steps: [{ goal: "Select Option 1 in the dropdown", assert: "() => document.querySelector('#dropdown').value === '1'" }] },
  { id: "m-hn-comments", cat: "touch-nav", url: "https://news.ycombinator.com/",
    steps: [{ goal: "Open the comments page of the top story", assert: "() => location.pathname === '/item'" }] },
  { id: "m-books-fiction", cat: "touch-nav", url: "https://books.toscrape.com/",
    steps: [{ goal: "Open the Fiction category", assert: "() => /\\/fiction_\\d+\\//.test(location.pathname)" }] },
  { id: "m-github-issues", cat: "touch-nav", url: "https://github.com/microsoft/playwright",
    steps: [{ goal: "Open the repository's Issues tab", assert: "() => /\\/issues\\/?$/.test(location.pathname)" }] },
  { id: "m-neg-pricing", cat: "negative", expect: "fail", url: MW + "Main_Page",
    steps: [{ goal: "Open the site's pricing page", assert: "() => location.pathname.includes('pricing')" }] },
];

// Carousels on other libraries' pages, written 2026-11-06 before act ran
// on them, to see how act does past one demo carousel. Bootstrap hides its
// other slides outright; the others keep them in the page.
const SLICK = "() => document.querySelector('.single-item').slick.currentSlide";
// The slides in view, in order: one on a phone, three on a desktop.
const OWL = "[...document.querySelector('.owl-carousel').querySelectorAll('.owl-item.active')].map(x => x.innerText.trim())";
const BS = "[...document.querySelectorAll('#carouselExample .carousel-item')].findIndex(x => x.classList.contains('active'))";
export const CAROUSEL = [
  { id: "c-slick-next", cat: "carousel", url: "https://kenwheeler.github.io/slick/",
    steps: [{ goal: "Go to the next slide in the Single Item slider", assert: `() => (${SLICK})() === 1` }] },
  { id: "c-slick-third", cat: "carousel", url: "https://kenwheeler.github.io/slick/",
    steps: [{ goal: "In the Single Item slider, show slide 3", assert: `() => (${SLICK})() === 2` }] },
  { id: "c-owl-drag", cat: "carousel", url: "https://owlcarousel2.github.io/OwlCarousel2/",
    steps: [{ goal: "Show the Drag slide in the demo carousel", assert: `() => ${OWL}.includes('Drag')` }] },
  { id: "c-owl-two", cat: "carousel", url: "https://owlcarousel2.github.io/OwlCarousel2/",
    steps: [{ goal: "Move the demo carousel forward two slides", assert: `() => ${OWL}[0] === 'Responsive'` }] },
  { id: "c-bs-second", cat: "carousel", url: "https://getbootstrap.com/docs/5.3/components/carousel/",
    steps: [{ goal: "Show the second slide of the first example carousel", assert: `() => ${BS} === 1` }] },
  { id: "c-bs-third", cat: "carousel", url: "https://getbootstrap.com/docs/5.3/components/carousel/",
    steps: [{ goal: "Show the Third slide in the first example carousel", assert: `() => ${BS} === 2` }] },
  // Added after the first carousel run, also before act ran on them: the
  // page has one carousel, so the goals can't mean another.
  { id: "c-owl-css3", cat: "carousel", url: "https://owlcarousel2.github.io/OwlCarousel2/",
    steps: [{ goal: "Show the CSS3 slide in the demo carousel", assert: `() => ${OWL}.includes('CSS3')` }] },
  { id: "c-owl-fast", cat: "carousel", url: "https://owlcarousel2.github.io/OwlCarousel2/",
    steps: [{ goal: "Show the Fast slide in the demo carousel", assert: `() => ${OWL}.includes('Fast')` }] },
  { id: "c-neg-slick-ten", cat: "negative", expect: "fail", url: "https://kenwheeler.github.io/slick/",
    steps: [{ goal: "In the Single Item slider, show slide 10", assert: `() => false` }] },
];
