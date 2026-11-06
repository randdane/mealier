use mealier::{Confidence, Dimension, Food, Unit, parse_line};

fn unit(id: i64, name: &str, aliases: &[&str]) -> Unit {
    Unit { id, name: name.into(), aliases: aliases.iter().map(|s| s.to_string()).collect(), dimension: Dimension::Volume, factor: Some(1.0) }
}
fn food(id: i64, name: &str, aliases: &[&str]) -> Food {
    Food { id, name: name.into(), aliases: aliases.iter().map(|s| s.to_string()).collect() }
}

// Unit ids: 1 cup, 2 tablespoon, 3 teaspoon, 4 gram, 5 fluid ounce, 6 ounce, 7 clove.
fn units() -> Vec<Unit> {
    vec![
        unit(1, "cup", &[]),
        unit(2, "tablespoon", &[]),
        unit(3, "teaspoon", &["tspn"]),
        unit(4, "gram", &[]),
        unit(5, "fluid ounce", &[]),
        unit(6, "ounce", &[]),
        unit(7, "clove", &["cloves"]),
    ]
}
// Food ids: 1 flour, 2 egg, 3 salt, 4 tomato, 5 olive oil, 6 oil, 7 scallion, 8 garlic.
fn foods() -> Vec<Food> {
    vec![
        food(1, "flour", &[]),
        food(2, "egg", &[]),
        food(3, "salt", &[]),
        food(4, "tomato", &[]),
        food(5, "olive oil", &[]),
        food(6, "oil", &[]),
        food(7, "scallion", &["green onion"]),
        food(8, "garlic", &[]),
    ]
}

/// (input, qty_min, qty_max, unit_id, food_id, note, confidence)
type Case = (&'static str, Option<f64>, Option<f64>, Option<i64>, Option<i64>, &'static str, Confidence);

fn check(cases: &[Case]) {
    for &(text, qmin, qmax, unit_id, food_id, note, confidence) in cases {
        let got = parse_line(text, &units(), &foods());
        let want = mealier::Parsed { qty_min: qmin, qty_max: qmax, unit_id, food_id, note: note.into(), original_text: text.into(), confidence };
        assert_eq!(got, want, "input {text:?}");
    }
}

use Confidence::{Full, Partial};

#[test]
fn quantities() {
    check(&[
        ("2 cups flour", Some(2.0), Some(2.0), Some(1), Some(1), "", Full),
        ("1.5 cups flour", Some(1.5), Some(1.5), Some(1), Some(1), "", Full),
        ("½ cup flour", Some(0.5), Some(0.5), Some(1), Some(1), "", Full),
        ("¼ cup flour", Some(0.25), Some(0.25), Some(1), Some(1), "", Full),
        ("¾ cup flour", Some(0.75), Some(0.75), Some(1), Some(1), "", Full),
        ("⅛ cup flour", Some(0.125), Some(0.125), Some(1), Some(1), "", Full),
        ("1/2 cup flour", Some(0.5), Some(0.5), Some(1), Some(1), "", Full),
        ("1 1/2 cups flour", Some(1.5), Some(1.5), Some(1), Some(1), "", Full),
        ("1½ cups flour", Some(1.5), Some(1.5), Some(1), Some(1), "", Full),
        ("2 ½ cups flour", Some(2.5), Some(2.5), Some(1), Some(1), "", Full),
        ("1-2 cups flour", Some(1.0), Some(2.0), Some(1), Some(1), "", Full),
        ("1 to 2 cups flour", Some(1.0), Some(2.0), Some(1), Some(1), "", Full),
        ("1–2 cups flour", Some(1.0), Some(2.0), Some(1), Some(1), "", Full),
        ("1 - 2 cups flour", Some(1.0), Some(2.0), Some(1), Some(1), "", Full),
        ("1-1/2 cups flour", Some(1.5), Some(1.5), Some(1), Some(1), "", Full),
        ("2 eggs", Some(2.0), Some(2.0), None, Some(2), "", Full),
    ]);
    let third = parse_line("⅓ cup flour", &units(), &foods());
    assert!((third.qty_min.unwrap() - 1.0 / 3.0).abs() < 1e-9);
    let two_thirds = parse_line("⅔ cup flour", &units(), &foods());
    assert!((two_thirds.qty_min.unwrap() - 2.0 / 3.0).abs() < 1e-9);
}

#[test]
fn units_by_name_plural_abbreviation_and_alias() {
    check(&[
        ("1 tablespoon flour", Some(1.0), Some(1.0), Some(2), Some(1), "", Full),
        ("2 tbsp flour", Some(2.0), Some(2.0), Some(2), Some(1), "", Full),
        ("2 Tbsp flour", Some(2.0), Some(2.0), Some(2), Some(1), "", Full),
        ("2 Tbsp. flour", Some(2.0), Some(2.0), Some(2), Some(1), "", Full),
        ("2 TABLESPOONS flour", Some(2.0), Some(2.0), Some(2), Some(1), "", Full),
        ("1 tsp salt", Some(1.0), Some(1.0), Some(3), Some(3), "", Full),
        ("1 tspn salt", Some(1.0), Some(1.0), Some(3), Some(3), "", Full),
        ("100g flour", Some(100.0), Some(100.0), Some(4), Some(1), "", Full),
        ("100 grams flour", Some(100.0), Some(100.0), Some(4), Some(1), "", Full),
        ("2 cups of flour", Some(2.0), Some(2.0), Some(1), Some(1), "", Full),
        // "fl oz" must win over "oz".
        ("8 fl oz olive oil", Some(8.0), Some(8.0), Some(5), Some(5), "", Full),
        ("8 oz flour", Some(8.0), Some(8.0), Some(6), Some(1), "", Full),
        // Single letters ("T", "c") are ambiguous and never match a unit.
        ("2 T flour", Some(2.0), Some(2.0), None, Some(1), "T", Full),
    ]);
}

#[test]
fn foods_by_name_alias_longest_match_and_number() {
    check(&[
        ("3 tomatoes", Some(3.0), Some(3.0), None, Some(4), "", Full),
        ("1 egg", Some(1.0), Some(1.0), None, Some(2), "", Full),
        ("2 Eggs", Some(2.0), Some(2.0), None, Some(2), "", Full),
        ("2 green onions", Some(2.0), Some(2.0), None, Some(7), "", Full),
        ("2 scallions", Some(2.0), Some(2.0), None, Some(7), "", Full),
        // Longest match: "olive oil" beats "oil".
        ("1 tbsp olive oil", Some(1.0), Some(1.0), Some(2), Some(5), "", Full),
        ("1 tbsp oil", Some(1.0), Some(1.0), Some(2), Some(6), "", Full),
        ("3 cloves garlic", Some(3.0), Some(3.0), Some(7), Some(8), "", Full),
        // Words around the food are kept as a note, not lost.
        ("2 large eggs", Some(2.0), Some(2.0), None, Some(2), "large", Full),
    ]);
}

#[test]
fn notes() {
    check(&[
        ("1 cup flour, sifted", Some(1.0), Some(1.0), Some(1), Some(1), "sifted", Full),
        ("2 tomatoes, chopped", Some(2.0), Some(2.0), None, Some(4), "chopped", Full),
        ("1 cup flour (optional)", Some(1.0), Some(1.0), Some(1), Some(1), "optional", Full),
        ("1 cup flour (sifted), divided", Some(1.0), Some(1.0), Some(1), Some(1), "sifted, divided", Full),
        ("salt to taste", None, None, None, Some(3), "to taste", Full),
        ("Salt, to taste", None, None, None, Some(3), "to taste", Full),
    ]);
}

#[test]
fn partial_and_unparseable() {
    check(&[
        // Quantity without a known food.
        ("2 cups zorblax", Some(2.0), Some(2.0), Some(1), None, "zorblax", Partial),
        // Food without quantity.
        ("flour", None, None, None, Some(1), "", Partial),
        ("", None, None, None, None, "", Confidence::None),
        ("   ", None, None, None, None, "", Confidence::None),
        ("zorblax", None, None, None, None, "", Confidence::None),
        ("For the sauce:", None, None, None, None, "", Confidence::None),
    ]);
}
