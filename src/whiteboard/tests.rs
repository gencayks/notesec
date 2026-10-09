use super::geom::*;
use super::*;

fn board_md() -> String {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    format!(
        "- type:: whiteboard\n  tags:: #plans\n\
         - Buy milk\n  id:: {a}\n  x:: 120\n  y:: 80\n  w:: 200\n  h:: 100\n  color:: Yellow\n\
         - [[Project]]\n  id:: {b}\n  x:: 400\n  y:: 80\n\
         - edges::\n  - edge:: (({a})) -> (({b}))\n    label:: next\n"
    )
}

#[test]
fn round_trip_through_the_page_file() {
    let md = board_md();
    let page = Page::from_markdown("Board", false, &md);
    assert!(is_whiteboard(&page));
    assert_eq!(page.to_markdown(), md, "a load and save changes nothing");
    let board = parse(&page);
    assert_eq!(board.cards.len(), 2);
    let milk = &board.cards[0];
    assert_eq!(milk.text, "Buy milk");
    assert_eq!(milk.rect, Rect::new(120.0, 80.0, 200.0, 100.0));
    assert_eq!(milk.color, Some("yellow"));
    assert_eq!(milk.kind, CardKind::Text);
    assert!(milk.placed);
    let project = &board.cards[1];
    assert_eq!(project.kind, CardKind::Page("Project".into()));
    assert_eq!(
        (project.rect.w, project.rect.h),
        (CARD_W, CARD_H),
        "defaults"
    );
    assert_eq!(board.edges.len(), 1);
    assert_eq!(board.edges[0].from, milk.id);
    assert_eq!(board.edges[0].to, project.id);
    assert_eq!(board.edges[0].label.as_deref(), Some("next"));

    // Edits, saved and loaded again.
    let mut page = page;
    let c = add_card(&mut page, "New idea", Rect::new(-50.4, 300.6, 220.0, 120.0));
    assert!(set_rect(
        &mut page,
        milk.id,
        Rect::new(10.0, 20.0, 300.0, 150.0)
    ));
    assert!(set_color(&mut page, c, Some("blue")));
    let e = connect(&mut page, c, milk.id).unwrap();
    assert_eq!(connect(&mut page, c, milk.id), None, "no duplicates");
    assert_eq!(connect(&mut page, c, c), None, "not to itself");
    let again = Page::from_markdown("Board", false, &page.to_markdown());
    let board = parse(&again);
    assert_eq!(board.cards.len(), 3);
    let new = board
        .card(c)
        .expect("the id was saved: an edge points at it");
    assert_eq!(new.text, "New idea");
    assert_eq!(new.rect, Rect::new(-50.0, 301.0, 220.0, 120.0));
    assert_eq!(new.color, Some("blue"));
    assert_eq!(
        board.card(milk.id).unwrap().rect,
        Rect::new(10.0, 20.0, 300.0, 150.0)
    );
    assert_eq!(board.edges.len(), 2);
    assert!(board
        .edges
        .iter()
        .any(|x| x.id == e || (x.from == c && x.to == milk.id)));
    // The card went before `edges::` in the file.
    let text = again.to_markdown();
    assert!(text.find("New idea").unwrap() < text.find("edges::").unwrap());

    // Deleting a card takes its edges.
    let mut page = again;
    assert!(delete_card(&mut page, milk.id));
    let board = parse(&page);
    assert_eq!(board.cards.len(), 2);
    assert!(board.edges.is_empty());
    assert!(!page.to_markdown().contains(&milk.id.to_string()));
}

#[test]
fn text_and_properties_stay_apart() {
    let content = "Line one\nx:: 1\nLine two\ny:: 2\ncolor:: red";
    assert_eq!(card_text(content), "Line one\nLine two");
    assert_eq!(
        with_text(content, "New\ntext"),
        "New\ntext\nx:: 1\ny:: 2\ncolor:: red"
    );
    assert_eq!(
        with_rect("Hi\ncolor:: red\nx:: 9", Rect::new(1.4, 2.6, 300.0, 50.0)),
        "Hi\ncolor:: red\nx:: 1\ny:: 3\nw:: 300\nh:: 50"
    );
    assert_eq!(with_color("Hi\ncolor:: red", None), "Hi");
    // Other properties are the card's own text.
    assert_eq!(card_text("Hi\nstatus:: done\nw:: 5"), "Hi\nstatus:: done");
}

#[test]
fn garbled_data_is_tolerated() {
    let id = Uuid::new_v4();
    let md = format!(
        "- type:: Whiteboard\n\
         - placed\n  x:: 0\n  y:: 0\n  w:: 100\n  h:: 100\n\
         - nan\n  x:: NaN\n  y:: 5\n\
         - huge\n  x:: 1e30\n  y:: -1e30\n  w:: -5\n  h:: 99999999\n\
         - words\n  x:: left\n  y::\n  w:: wide\n\
         - no props at all\n\
         - x:: 5\n\
         - edge:: (({id})) -> ((not-a-uuid))\n\
         - edges::\n  - edge:: nonsense\n  - edge:: (({id})) -> (({id}))\n  - stray child\n\
         - [[]]\n  x:: 0\n  y:: 400\n"
    );
    let page = Page::from_markdown("G", false, &md);
    assert!(is_whiteboard(&page));
    let board = parse(&page);
    let texts: Vec<&str> = board.cards.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "placed",
            "nan",
            "huge",
            "words",
            "no props at all",
            "",
            "[[]]"
        ]
    );
    assert!(board.edges.is_empty());
    for card in &board.cards {
        let r = card.rect;
        assert!(r.x.is_finite() && r.y.is_finite(), "{card:?}");
        assert!((MIN_W..=MAX_SIZE).contains(&r.w) && (MIN_H..=MAX_SIZE).contains(&r.h));
    }
    assert_eq!(board.cards[6].kind, CardKind::Text, "an empty link is text");
    // Unplaced cards go below the placed ones, without overlapping.
    let unplaced: Vec<&Card> = board.cards.iter().filter(|c| !c.placed).collect();
    assert_eq!(unplaced.len(), 5);
    let bottom = board
        .cards
        .iter()
        .filter(|c| c.placed)
        .map(|c| c.rect.y + c.rect.h)
        .fold(0.0, f32::max);
    for (i, a) in unplaced.iter().enumerate() {
        assert!(a.rect.y >= bottom, "{a:?}");
        for b in &unplaced[i + 1..] {
            assert!(!a.rect.intersects(&b.rect), "{a:?} {b:?}");
        }
    }
    // Not a whiteboard at all: still parses (as the outline's blocks).
    let plain = Page::from_markdown("P", false, "- a\n  - b\n- c\n");
    assert!(!is_whiteboard(&plain));
    assert_eq!(parse(&plain).cards.len(), 2);
    assert_eq!(parse(&Page::new("E", false)), Board::default());
}

#[test]
fn block_cards_and_new_pages() {
    let id = Uuid::new_v4();
    let mut page = new_page("Fresh");
    assert!(is_whiteboard(&page));
    assert_eq!(page.to_markdown(), "- type:: whiteboard\n");
    add_card(
        &mut page,
        &format!("(({id}))"),
        Rect::new(0.0, 0.0, 200.0, 80.0),
    );
    add_card(
        &mut page,
        &format!("see (({id}))"),
        Rect::new(0.0, 0.0, 200.0, 80.0),
    );
    let board = parse(&page);
    assert_eq!(board.cards[0].kind, CardKind::Block(id));
    assert_eq!(board.cards[1].kind, CardKind::Text);
    assert!(!delete_edge(&mut page, board.cards[0].id), "not an edge");
}

#[test]
fn zoom_keeps_the_point_under_the_cursor() {
    let mut view = Viewport {
        zoom: 1.0,
        offset: pt(100.0, 50.0),
    };
    let at = pt(400.0, 300.0);
    let under = view.to_world(at);
    view.zoom_at(at, 2.0);
    assert_eq!(view.zoom, 2.0);
    let after = view.to_world(at);
    assert!((after.x - under.x).abs() < 1e-3 && (after.y - under.y).abs() < 1e-3);
    // Clamped both ways, still around the point.
    view.zoom_at(at, 1000.0);
    assert_eq!(view.zoom, MAX_ZOOM);
    view.zoom_at(at, 0.00001);
    assert_eq!(view.zoom, MIN_ZOOM);
    let after = view.to_world(at);
    assert!((after.x - under.x).abs() < 1e-2 && (after.y - under.y).abs() < 1e-2);
    view.zoom_at(at, f32::NAN);
    assert_eq!(view.zoom, MIN_ZOOM);
    // Round trips.
    let p = pt(-123.0, 456.0);
    let back = view.to_world(view.to_screen(p));
    assert!((back.x - p.x).abs() < 1e-3 && (back.y - p.y).abs() < 1e-3);
}

#[test]
fn fit_shows_everything() {
    let rects = [
        Rect::new(0.0, 0.0, 100.0, 100.0),
        Rect::new(1900.0, 900.0, 100.0, 100.0),
    ];
    let view = Viewport::fit(&rects, 1000.0, 600.0);
    for r in rects {
        let s = view.rect_to_screen(r);
        assert!(
            s.x >= 0.0 && s.y >= 0.0 && s.x + s.w <= 1000.0 && s.y + s.h <= 600.0,
            "{s:?}"
        );
    }
    // Small boards aren't blown up past 100%.
    let one = Viewport::fit(&[Rect::new(5.0, 5.0, 50.0, 50.0)], 1000.0, 600.0);
    assert_eq!(one.zoom, 1.0);
    assert_eq!(one.to_screen(pt(30.0, 30.0)), pt(500.0, 300.0), "centred");
    assert_eq!(Viewport::fit(&[], 10.0, 10.0), Viewport::default());
}

#[test]
fn edges_anchor_on_card_borders() {
    let a = Rect::new(0.0, 0.0, 100.0, 50.0);
    let b = Rect::new(300.0, 0.0, 100.0, 50.0);
    assert_eq!(anchors(a, b), (pt(100.0, 25.0), pt(300.0, 25.0)));
    let below = Rect::new(0.0, 200.0, 100.0, 50.0);
    assert_eq!(anchors(a, below), (pt(50.0, 50.0), pt(50.0, 200.0)));
    // Diagonal: on the border, between the centres.
    let d = Rect::new(300.0, 300.0, 100.0, 50.0);
    let (p, q) = anchors(a, d);
    assert!(
        (p.x - 100.0).abs() < 1e-3 || (p.y - 50.0).abs() < 1e-3,
        "{p:?}"
    );
    assert!(
        (q.x - 300.0).abs() < 1e-3 || (q.y - 300.0).abs() < 1e-3,
        "{q:?}"
    );
    // Same centre: no direction, the centre.
    assert_eq!(a.border_point(a.center()), a.center());
    let head = arrowhead(pt(0.0, 0.0), pt(100.0, 0.0));
    assert_eq!(head[0], pt(100.0, 0.0));
    assert_eq!(head[1], pt(100.0 - ARROW_LEN, ARROW_HALF_WIDTH));
    assert_eq!(head[2], pt(100.0 - ARROW_LEN, -ARROW_HALF_WIDTH));
    assert_eq!(arrowhead(pt(1.0, 1.0), pt(1.0, 1.0)), [pt(1.0, 1.0); 3]);
}

fn simple_board() -> (Board, Uuid, Uuid, Uuid) {
    let mut page = new_page("B");
    let a = add_card(&mut page, "a", Rect::new(0.0, 0.0, 100.0, 100.0));
    let b = add_card(&mut page, "[[P]]", Rect::new(300.0, 0.0, 100.0, 100.0));
    // Overlapping a: drawn on top.
    let c = add_card(&mut page, "c", Rect::new(50.0, 50.0, 100.0, 100.0));
    connect(&mut page, a, b).unwrap();
    (parse(&page), a, b, c)
}

#[test]
fn hit_testing() {
    let (board, a, b, c) = simple_board();
    let view = Viewport {
        zoom: 2.0,
        offset: pt(10.0, 10.0),
    };
    let at = |x: f32, y: f32| view.to_screen(pt(x, y));
    assert_eq!(hit(&board, &view, at(10.0, 10.0), None), Hit::Card(a));
    assert_eq!(
        hit(&board, &view, at(90.0, 90.0), None),
        Hit::Card(c),
        "topmost"
    );
    assert_eq!(hit(&board, &view, at(350.0, 10.0), None), Hit::PageTitle(b));
    assert_eq!(hit(&board, &view, at(350.0, 60.0), None), Hit::Card(b));
    let edge = board.edges[0].id;
    // The arrow runs from a's right border to b's left one, at y = 50
    // (centres): 4 screen px away still counts, 10 doesn't.
    assert_eq!(hit(&board, &view, at(250.0, 50.0), None), Hit::Edge(edge));
    let near = at(250.0, 50.0);
    assert_eq!(
        hit(&board, &view, pt(near.x, near.y + 4.0), None),
        Hit::Edge(edge)
    );
    assert_eq!(
        hit(&board, &view, pt(near.x, near.y + 10.0), None),
        Hit::Empty
    );
    assert_eq!(hit(&board, &view, at(-50.0, -50.0), None), Hit::Empty);
    // Handles only on the selected card.
    let s = view.rect_to_screen(board.card(c).unwrap().rect);
    let corner = pt(s.x + s.w - 2.0, s.y + s.h - 2.0);
    assert_eq!(hit(&board, &view, corner, None), Hit::Card(c));
    assert_eq!(hit(&board, &view, corner, Some(c)), Hit::Resize(c));
    let dot = pt(s.x + s.w + 3.0, s.y + s.h / 2.0);
    assert_eq!(hit(&board, &view, dot, Some(c)), Hit::Connect(c));
    assert_eq!(hit(&board, &view, dot, None), Hit::Empty);
}

#[test]
fn culling_a_large_board() {
    // 500 cards in a 25 × 20 grid, 300 units apart.
    let mut page = new_page("Big");
    for i in 0..500 {
        let (col, row) = ((i % 25) as f32, (i / 25) as f32);
        add_card(
            &mut page,
            &format!("card {i}"),
            Rect::new(col * 300.0, row * 300.0, 200.0, 100.0),
        );
    }
    let board = parse(&page);
    assert_eq!(board.cards.len(), 500);
    // At 100% a 1200×800 window over the corner: 4 columns × 3 rows (a
    // row/column partly in view counts).
    let view = Viewport {
        zoom: 1.0,
        offset: pt(0.0, 0.0),
    };
    let shown = visible_cards(&board, &view, 1200.0, 800.0);
    assert_eq!(shown.len(), 4 * 3);
    assert!(shown
        .iter()
        .all(|&i| board.cards[i].rect.x < 1200.0 && board.cards[i].rect.y < 800.0));
    // Panned into the middle.
    let view = Viewport {
        zoom: 1.0,
        offset: pt(-3000.0, -3000.0),
    };
    let shown = visible_cards(&board, &view, 1200.0, 800.0);
    assert_eq!(shown.len(), 4 * 3);
    // Fit all: everything, at a zoom that still respects the clamp.
    let fit = Viewport::fit(&board.rects(), 1200.0, 800.0);
    assert!(fit.zoom >= MIN_ZOOM && fit.zoom < 1.0);
    assert_eq!(visible_cards(&board, &fit, 1200.0, 800.0).len(), 500);
    // Far away: nothing.
    let away = Viewport {
        zoom: 1.0,
        offset: pt(1e6, 1e6),
    };
    assert!(visible_cards(&board, &away, 1200.0, 800.0).is_empty());
}

#[test]
fn export_draws_the_board_as_inline_svg() {
    let mut md = board_md();
    md.push_str("- <script>alert(1)</script> [x](javascript:alert(1))\n  x:: 0\n  y:: 400\n");
    let page = Page::from_markdown("Board", false, &md);
    let html = crate::export::page_html(&page, &|_| None, &|_| None);
    assert!(html.contains("<svg"), "{html}");
    assert!(html.contains("<foreignObject"));
    assert!(html.contains("class=\"edge\"") && html.contains("class=\"arrow\""));
    assert!(html.contains("Buy milk") && html.contains(">next<"));
    // Card properties are not text, and nothing runs.
    assert!(!html.contains("x:: 120"));
    assert!(!html.contains("<script"));
    assert!(html.contains("&lt;script&gt;"), "shown as text");
    assert!(!html.contains("href=\"javascript"));
    assert!(html.contains("default-src 'none'"));
    assert!(!html.contains("<ul class=\"outline\">"), "the board alone");
}

#[test]
fn published_boards_need_no_inline_styles() {
    let dir = std::env::temp_dir().join(format!("notesec-wb-publish-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let pages = vec![
        Page::from_markdown("Board", false, &board_md()),
        Page::from_markdown("Project", false, "- the plan\n"),
    ];
    let bundle = crate::publish::build(&dir, &pages, 0, true).unwrap();
    let index = bundle
        .files
        .iter()
        .find(|(n, _)| n == "index.html")
        .unwrap();
    let html = String::from_utf8(index.1.clone()).unwrap();
    assert!(html.contains("<svg") && html.contains("Buy milk"));
    // The publish CSP allows no inline style: no style attributes at all.
    assert!(html.contains("style-src 'self'"));
    assert!(!html.contains("style="), "{html}");
    assert!(!html.contains("<script"));
    assert!(!html.contains("x:: "));
    assert_eq!(bundle.pages, vec![0, 1], "the page card's page goes along");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cards_count_as_links() {
    let pages = vec![
        Page::from_markdown("Board", false, &board_md()),
        Page::from_markdown("Project", false, "- the plan\n"),
    ];
    let backlinks = crate::model::backlinks(&pages, 1);
    assert_eq!(backlinks.len(), 1);
    assert_eq!(pages[backlinks[0].page].title, "Board");
    let graph = crate::graph::Graph::build(&pages, true);
    let project = graph
        .nodes
        .iter()
        .position(|n| n.title == "Project")
        .unwrap();
    assert_eq!(graph.nodes[project].backlinks, 1);
}
