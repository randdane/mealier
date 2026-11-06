//! Unit names and abbreviations shared by the Mealie importer and the ingredient parser.
use crate::Dimension;

/// Mass and volume units Mealie ships, by lowercase name or abbreviation:
/// (names, dimension, grams or millilitres per unit).
pub(crate) const KNOWN_UNITS: &[(&[&str], Dimension, f64)] = &[
    (&["g", "gram", "grams"], Dimension::Mass, 1.0),
    (&["kg", "kilogram", "kilograms"], Dimension::Mass, 1000.0),
    (&["mg", "milligram", "milligrams"], Dimension::Mass, 0.001),
    (&["oz", "ounce", "ounces"], Dimension::Mass, 28.349523125),
    (&["lb", "lbs", "pound", "pounds"], Dimension::Mass, 453.59237),
    (&["ml", "milliliter", "milliliters", "millilitre", "millilitres"], Dimension::Volume, 1.0),
    (&["l", "liter", "liters", "litre", "litres"], Dimension::Volume, 1000.0),
    (&["tsp", "teaspoon", "teaspoons"], Dimension::Volume, 4.92892159375),
    (&["tbsp", "tablespoon", "tablespoons"], Dimension::Volume, 14.78676478125),
    (&["c", "cup", "cups"], Dimension::Volume, 236.5882365),
    (&["fl oz", "fluid ounce", "fluid ounces"], Dimension::Volume, 29.5735295625),
    (&["pt", "pint", "pints"], Dimension::Volume, 473.176473),
    (&["qt", "quart", "quarts"], Dimension::Volume, 946.352946),
    (&["gal", "gallon", "gallons"], Dimension::Volume, 3785.411784),
];
