//! Box-model geometry calculator and click dispatcher.

use flashbrowse_cdp::protocol::dom::BoxModel;

/// Center coordinate of a box `border` quad `[x1,y1,x2,y2,x3,y3,x4,y4]`.
pub fn box_center(model: &BoxModel) -> Option<(f64, f64)> {
    let b = &model.border;
    let [x1, y1, x2, y2, x3, y3, x4, y4, ..] = b.as_slice() else {
        return None;
    };
    let x = (x1 + x2 + x3 + x4) / 4.0;
    let y = (y1 + y2 + y3 + y4) / 4.0;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some((x, y))
}

#[cfg(test)]
mod tests {
    use super::box_center;
    use flashbrowse_cdp::protocol::dom::BoxModel;

    fn model(border: Vec<f64>) -> BoxModel {
        BoxModel {
            content: border.clone(),
            border,
            margin: vec![],
            padding: vec![],
            width: 100.0,
            height: 50.0,
        }
    }

    #[test]
    fn center_of_rect() {
        let m = model(vec![0.0, 0.0, 100.0, 0.0, 100.0, 50.0, 0.0, 50.0]);
        let (x, y) = box_center(&m).unwrap();
        assert_eq!(x, 50.0);
        assert_eq!(y, 25.0);
    }

    #[test]
    fn rejects_short_quad() {
        let m = model(vec![0.0, 0.0, 10.0]);
        assert!(box_center(&m).is_none());
    }
}
